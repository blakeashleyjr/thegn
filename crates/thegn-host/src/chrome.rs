//! In-process chrome: the four surfaces (tabbar, sidebar, panel, statusbar)
//! drawn natively into the back-buffer `Surface` around the center pane. No
//! WASM, no plugin hop, no broadcast — widgets read state directly and draw cells.
//! This replaces the four zellij plugins.

use termwiz::cell::AttributeChange;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::compositor::{Rect, compose_pane};
use crate::emulator::PaneEmulator;
use thegn_core::theme;
use unicode_width::UnicodeWidthStr;

// The masthead (top bar) layout + hit-test spans live in the `masthead` sibling
// module (seg-layer, mirrors the statusbar); re-exported so callers that reach
// for `chrome::masthead_item_spans` keep resolving.
pub use crate::masthead::masthead_item_spans;

/// The resolved chrome palette. A process-global because every draw helper
/// needs it and threading it through each call would touch every signature;
/// the event loop writes it (startup + config reload), render-time code only
/// reads. Defaults match the built-in storm-blue theme.
static PALETTE: std::sync::LazyLock<std::sync::RwLock<theme::Palette>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(theme::Palette::default()));

/// Every slot pre-resolved to a termwiz color, rebuilt on [`set_palette`].
/// [`col`] is called thousands of times per Full frame (per row / per seg
/// across the whole chrome); resolving used to split the live palette's
/// `"r;g;b"` string and run three integer parses on every call.
static RESOLVED: std::sync::LazyLock<std::sync::RwLock<[ColorAttribute; S::COUNT]>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(resolve_slots(&theme::Palette::default())));

fn resolve_slots(p: &theme::Palette) -> [ColorAttribute; S::COUNT] {
    S::ALL.map(|s| theme_color(slot_rgb(p, s)))
}

/// Install the resolved palette (startup and live config reload).
pub fn set_palette(p: theme::Palette) {
    let resolved = resolve_slots(&p);
    if let Ok(mut g) = PALETTE.write() {
        *g = p;
    }
    if let Ok(mut g) = RESOLVED.write() {
        *g = resolved;
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
    /// The alternate list tint. Sidebar project blocks alternate between this
    /// and [`S::Panel`] so two abutting projects read as two blocks.
    PanelAlt,
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
    ActivityDone,
}

impl S {
    /// Every slot, in discriminant order — indexes [`RESOLVED`].
    pub const ALL: [S; 21] = [
        S::Bg0,
        S::Bg1,
        S::Panel,
        S::PanelAlt,
        S::Panel2,
        S::Raise,
        S::Border,
        S::Focus,
        S::Text,
        S::Dim,
        S::Faint,
        S::Ghost,
        S::Ghost2,
        S::Ghost3,
        S::ShadowBg,
        S::ShadowFg,
        S::ChipFg,
        S::Accent,
        S::ActivityActive,
        S::ActivityWaiting,
        S::ActivityDone,
    ];
    pub const COUNT: usize = Self::ALL.len();
}

/// The "R;G;B" fragment for a slot within a palette (shared by [`col`] and
/// the seg layer's one-lock-per-line resolution).
pub fn slot_rgb(p: &theme::Palette, s: S) -> &str {
    match s {
        S::Bg0 => &p.bg0,
        S::Bg1 => &p.bg1,
        S::Panel => &p.panel,
        S::PanelAlt => &p.panel_alt,
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
        S::ActivityDone => &p.activity_done,
    }
}

/// Resolve a palette slot to a termwiz color — an array index into the
/// pre-resolved cache (rebuilt by [`set_palette`]), not a string parse.
pub fn col(s: S) -> ColorAttribute {
    RESOLVED.read().expect("palette lock")[s as usize]
}

/// A palette slot as an sRGB triple, for the callers that must *compute* with
/// a color — swap it, blend it, compare it — rather than just emit it.
/// [`col`]'s `ColorAttribute` is opaque once resolved, so those callers would
/// otherwise have to carry their own literals past the chokepoint.
pub fn col_rgb(s: S) -> (u8, u8, u8) {
    with_palette(|p| {
        let mut it = slot_rgb(p, s)
            .split(';')
            .filter_map(|n| n.trim().parse::<u8>().ok());
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    })
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

/// Draw a transport-neutral plugin [`View`](thegn_core::plugin_api::View)
/// into a host-owned surface rect.
/// Plugins supply semantic roles only; this function resolves them against the
/// current thegn theme/accent and clips to the host-owned slot.
///
/// Wired into the live chrome by [`draw_statusbar`], which renders every
/// `StatusBarSegment` view from [`FrameModel::plugin_segments`].
pub fn draw_plugin_view(
    surface: &mut Surface,
    rect: Rect,
    view: &thegn_core::plugin_api::View,
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

fn plugin_role_color(role: thegn_core::plugin_api::StyleRole, accent_rgb: &str) -> ColorAttribute {
    use thegn_core::plugin_api::StyleRole;
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

/// The state of the always-on daemon indicator (far-right statusbar chip). One
/// glyph, no word — see `statusbar_badges::push_daemon_chip`. Computed each
/// frame from the cached [`DaemonStatus`] + whether the focused pane is
/// daemon-backed (`handlers::status::daemon_chip_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonChipState {
    /// The focused pane runs inline; quitting the UI ends it. Plain default.
    #[default]
    NonPersist,
    /// The focused pane is daemon-backed — quit detaches, relaunch reattaches.
    Persist,
    /// This instance's daemon is serving remote thin clients (`thegn serve`).
    Server,
    /// This instance is attached to a *remote* pane daemon (remote backend).
    Client,
    /// The daemon this instance expects is registered but its heartbeat has gone
    /// stale (crashed or wedged) — an error, not an unremarkable stale value.
    /// Activating the chip runs the on-demand probe, which distinguishes an
    /// unreachable socket from an alive-but-stale daemon.
    Error,
}

/// A cached, render-side view of the pane daemon this instance talks to,
/// refreshed off-loop (never a DB read on the loop) and delivered like any
/// other hydration producer. Feeds the far-right status chip's state and the
/// expanded status modal's identity/health block.
///
/// This is the **registry row**, and nothing more. Session counts deliberately
/// do NOT live here: they were once derived from the lease table, which only
/// records *detached* sessions, so an actively-served daemon reported zero. The
/// daemon's in-memory registry is the source of truth and is fetched over the
/// control socket on demand — see `detail::status_modal::DaemonSessions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonStatus {
    /// True once a daemon row has been resolved (else the modal shows a hint).
    pub present: bool,
    /// The daemon process PID.
    pub pid: Option<u32>,
    /// The daemon's reported `CARGO_PKG_VERSION`.
    pub version: String,
    /// Hostname the daemon runs on.
    pub hostname: String,
    /// Unix socket path / named-pipe endpoint the daemon binds.
    pub endpoint: String,
    /// `host:port` when the daemon is serving remote thin clients, else empty.
    pub tcp_addr: String,
    /// Daemon start time (Unix ms), for the uptime line.
    pub started_at_ms: i64,
    /// Last registry heartbeat (Unix ms). Age against `now` is the daemon's
    /// liveness signal — stale beyond `DAEMON_HEARTBEAT_TTL_MS`.
    pub heartbeat_at: i64,
    /// A daemon row for this scope exists but its heartbeat is past the TTL — a
    /// crashed or wedged daemon. Drives the chip's error state. Distinct from
    /// `present` (a *live* daemon): a stale row sets `stale` with `present` false.
    pub stale: bool,
    /// The daemon's registry id (its own random handle, distinct from the PID).
    pub daemon_id: String,
    /// Scope key: the canonicalized state dir this daemon serves.
    pub scope: String,
    /// True when this instance reaches the daemon over a *remote* transport
    /// (drives the `Client` chip state).
    pub remote: bool,
}

/// Renderer-neutral projection of the selected live frontend preview target.
/// The host supervisor owns lifecycle; chrome and panel code only paint this
/// memory-only snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewView {
    pub worktree: String,
    pub port: u16,
    pub url: String,
    pub source: thegn_core::preview::PortHintSource,
    pub status: thegn_core::preview::PreviewStatus,
}

/// What the chrome needs to paint a frame. Populated from session state + DB +
/// git by the host; kept renderer-agnostic so it's unit-testable.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct FrameModel {
    /// The active worktree group's name ("app/feat") — the tabbar's left label.
    pub worktree: String,
    /// App-wide network connectivity (from [`thegn_core::connectivity`]). Drives
    /// the offline statusbar chip; refreshed each hydrate.
    pub connectivity: thegn_core::connectivity::Connectivity,
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
    /// focus indicator in [`crate::sidebar_view::draw_sidebar`]; the
    /// Alt/Ctrl+1..9 quick-jump digits are painted only while focused — the
    /// gutter itself is always reserved so rows don't shift on focus).
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
    pub sidebar_menu: Option<crate::sidebar_view::RowMenu>,
    /// Top visible-row index of the scroll window (clamped by `build_sidebar`
    /// so the cursor stays in view). Mirrors `SidebarState::scroll`.
    pub sidebar_scroll: usize,
    /// True when the sidebar is in its slim collapsed rail mode (activity dots
    /// + initials only); false renders the full panel.
    pub sidebar_rail: bool,
    /// Resolved worktree-row display options (from `[ui]` config): which right-
    /// cluster fields show, the focused-detail policy, and icon overrides.
    pub sidebar_display: crate::sidebar_view::SidebarDisplay,
    /// Live sidebar drag feedback (source lift + drop affordance). Mouse-
    /// gesture-transient: set per drag sample, cleared on release/cancel;
    /// never part of `hydration_eq` (a mid-gesture model swap just clears it
    /// until the next motion sample).
    pub sidebar_drag: Option<crate::sidebar_view::SidebarDragViz>,
    /// Row heights frozen while a sidebar drag gesture is armed or active, so
    /// the rows cannot reflow under a held pointer (see
    /// [`crate::sidebar_view::SidebarLayoutLock`]). Mouse-gesture-transient like
    /// `sidebar_drag`, and re-stamped from `MouseUi` every loop iteration
    /// because a hydration swaps the whole model mid-gesture.
    pub sidebar_drag_lock: Option<crate::sidebar_view::SidebarLayoutLock>,
    /// Live center-pane drag-rearrange feedback: the drop target the pointer is
    /// over and whether it would swap or re-anchor. Mouse-gesture-transient like
    /// `sidebar_drag` (set per motion sample, cleared on release/cancel, never
    /// part of `hydration_eq`); `render_panes` paints the highlight from it.
    /// Border-resize drags carry no viz — the live weight change repaints the
    /// frame on its own.
    pub pane_drag: Option<crate::pane_drag::PaneDropViz>,
    /// Pending `thegn open` focus intents claimed from the DB mailbox by
    /// this hydration pass. Drained by the run-loop model drain BEFORE the
    /// model swap (never rendered, never part of `hydration_eq`).
    pub intents: Vec<thegn_core::store::IntentRow>,
    /// Pending `open --preset` launch intents claimed from the DB mailbox this
    /// pass. Applied right after the focus intent (name only — the preset
    /// resolves from this instance's own config). Same loop-drain contract as
    /// `intents`: never rendered, never part of `hydration_eq`.
    pub preset_intents: Vec<thegn_core::store::IntentRow>,
    /// Pending `adopt_session` intents claimed from the DB mailbox this pass —
    /// "a daemon session exists that no pane is showing; graft it in". Unlike
    /// the two above these are **all** applied, not last-wins: a pipeline that
    /// fans out eight stage agents writes eight rows and expects eight panes.
    /// Same loop-drain contract otherwise (never rendered, never part of
    /// `hydration_eq`).
    pub adopt_intents: Vec<thegn_core::store::IntentRow>,
    /// Pending `editor.open` requests claimed from the control mailbox. All
    /// rows are drained before model swap and revalidated by `ide_handoff`;
    /// this carrier is never rendered or compared by `hydration_eq`.
    pub open_editor_intents: Vec<thegn_core::store::IntentRow>,
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
    pub sidebar_db_folders: Vec<thegn_core::models::FolderRow>,
    /// All terminals, straight from DB, used by row builder.
    pub sidebar_db_terminals: Vec<thegn_core::models::TerminalRow>,
    /// `[disk].warn_threshold_gb`: the worktree-usage warning threshold (GiB)
    /// used by the `thegn disk` CLI. The statusbar disk badge is now a low
    /// *free-space* alert (`stats.disk_free_pct` vs `[stats]` thresholds), not a
    /// usage-sum trip. Config-derived, set in `build_model`.
    pub disk_warn_threshold_gb: u64,
    /// `[monitor] processes` **inverted** — true when the user turned process
    /// sampling off. Mirrored onto the model because `ProcSnapshot::default()`
    /// has `enabled: false`, so the tab could not tell "the user turned it off"
    /// from "the first sample has not landed yet" and told the user their config
    /// said something it did not.
    ///
    /// Stored inverted (rather than as `procs_enabled`) because `FrameModel`
    /// derives `Default` across ~120 fields: a `bool` defaults to `false`, and
    /// the default here must mean **enabled** to match
    /// `MonitorConfig::default().processes` — otherwise every frame before the
    /// first `build_model` reasserts exactly the lie this field exists to stop.
    /// Read it through [`FrameModel::procs_enabled`], never directly.
    /// Config-derived, set in `build_model` beside `disk_warn_threshold_gb`.
    pub procs_disabled: bool,
    /// Active worktree's total size (bytes), for the bottom `disk` widget next
    /// to LOC. From the off-loop scan cache; `None` until first scanned.
    pub active_worktree_disk: Option<u64>,
    /// Do-not-disturb active (item 426): drives the statusbar DND chip. Set each
    /// frame from the notification runtime.
    pub notify_dnd: bool,
    /// Focused pane is daemon-backed — quitting detaches it (the process keeps
    /// running; the next launch reattaches). One input to `daemon_state`.
    /// Set each frame from the live panes table.
    pub persistent_pane: bool,
    /// How many of this instance's live panes are daemon-backed, and how many
    /// panes there are in total. The status modal's "panes here" row: what share
    /// of what *this* UI shows would survive a quit. Set each frame alongside
    /// `persistent_pane` from the same panes table.
    pub daemon_panes: usize,
    pub pane_count: usize,
    /// Resolved state of the always-on far-right daemon indicator. Computed each
    /// frame from `persistent_pane` + the cached [`DaemonStatus`]. Drives the
    /// glyph the chip shows (`handlers::status::daemon_chip_state`).
    pub daemon_state: DaemonChipState,
    /// Active notification routing mode (item 427; `""` = default). Shown as a
    /// statusbar chip when non-empty.
    pub notify_mode: String,
    /// Loop-readable, in-memory delivery counters for the Monitor view.
    pub notification_delivery: crate::notification_delivery::DeliverySnapshot,
    /// Whether at least one effective named push sink is configured. Kept
    /// separate from the snapshot because an idle sink has all-zero counters.
    pub notification_delivery_configured: bool,
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
    /// True while the focused pane is maximized within the chrome — the middle
    /// stop of Ctrl+Alt+z (statusbar indicator, distinct from `zoomed`).
    pub maximized: bool,
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
    /// The sidebar's NAVIGATE footer rows as (chord, label) pairs. Built by
    /// `sidebar_keytable::footer_hints` — registry-derived jump chords (so they
    /// follow rebinds) plus the sidebar key table's advertised keys. Carried on
    /// the model because the pure row composer has no access to `Config`.
    pub sidebar_hints: Vec<(String, String)>,
    /// The idle splash's keybind block as (chord, label) pairs, derived from
    /// the registry (`logotype::splash_hints`) so a rebind updates the splash
    /// instead of leaving a stale literal on the first screen users ever see.
    pub splash_hints: Vec<crate::logotype::SplashHint>,
    /// Resolved chords for actions the chrome names inline, keyed by action id.
    /// Rebuilt per frame from the effective keymap by `run.rs`, so a draw site
    /// can print the live chord (`model.chord("help")`) instead of a literal
    /// that goes stale on rebind. Use `keyhints`/`sidebar_hints` for whole
    /// hint strips; this is for one-off mentions.
    pub chord_hints: std::collections::BTreeMap<String, String>,
    /// The input-mode chip letter for the statusbar ("N", "V", "I", "E").
    pub mode_chip: String,
    /// Latest system stats reading for the top bar.
    pub stats: thegn_metrics::StatsSnapshot,
    /// Latest per-process reading, for the monitor's Processes tab. Empty
    /// unless that tab is open — enumerating processes is gated, so this is
    /// deliberately absent the rest of the time rather than stale.
    pub procs: thegn_metrics::ProcSnapshot,
    /// Latest Prometheus scrape state for the sidebar metrics section.
    pub metrics: crate::metrics::MetricsState,
    /// Latest AI-account usage gather (`[usage]`) — one row per tracked account,
    /// feeding the statusbar badge, the System ▸ Usage panel section, and the
    /// `open-usage` overlay. Empty until the first poll, and while usage is off.
    ///
    /// Loop-owned like `stats` and `metrics`: pushed by the usage poller, never
    /// produced by hydration. It lives here rather than on `PanelData` for the
    /// same reason `stats` does — `PanelData` is the hydration payload and
    /// derives `Eq`, which a percentage-bearing type cannot honestly implement.
    pub usage: Vec<thegn_core::usage::AccountUsage>,
    /// `[usage]` mirrored into the model, so the badge and panel section can
    /// read the statusbar toggle and the warn/crit thresholds at draw time —
    /// the same arrangement as `bars` and `stats_icons`.
    pub usage_cfg: thegn_core::config::UsageConfig,
    /// Latest weather reading (`[weather]`). `None` while disabled, before the
    /// first delivery, or once hard-expired.
    ///
    /// Loop-owned like `stats` and `usage`: pushed by the weather task, never by
    /// hydration, so it survives a model swap.
    pub weather: Option<thegn_core::weather::WeatherSnapshot>,
    /// `[weather]` mirrored into the model, so the widget and the popup row read
    /// thresholds/units without a config handle (the `usage_cfg` precedent).
    pub weather_cfg: thegn_core::config_weather::WeatherConfig,
    /// Recent per-window history, keyed by `detail::history_key` — the Usage
    /// section's trend sparkline and the exhaustion forecast read it. Empty when
    /// `[usage] history_days = 0`.
    pub usage_history: std::collections::BTreeMap<String, Vec<(i64, f32)>>,
    /// Host-wide transcript token rollup (`[usage] token_rollups`). Refreshed on
    /// its own long cadence, so the loop keeps the last one between scans rather
    /// than blanking the block.
    pub usage_tokens: Option<crate::detail::TokenRollupView>,
    /// Model-proxy spend rollup (`[model_proxy]`), hydrated off-loop from the
    /// audit tables on the usage-refresh cadence. `None` when the proxy is
    /// disabled — the usage panel's proxy-spend block is then absent.
    pub model_proxy_spend: Option<thegn_core::proxy::stats::Rollup>,
    /// tokei per-language report for the active worktree (bottom-bar widget +
    /// detail table).
    pub loc: Option<thegn_core::loc::LocReport>,
    /// Widget-bar layout (`[bars]`) and stat icons (`[stats]`).
    pub bars: thegn_core::config::BarsConfig,
    pub stats_icons: thegn_core::config::StatsConfig,
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
    /// Selected frontend preview for the active worktree. Loop-owned and
    /// re-applied after hydration, like shares and forwards.
    pub preview: Option<PreviewView>,
    /// Deterministic container name for the active worktree path. The sandbox
    /// panel uses this to show the sandbox for the selected worktree instead of
    /// the first thegn-owned container on the machine.
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
    /// Running containers (thegn-owned first) for the SANDBOXES section.
    pub containers: Vec<thegn_core::sandbox::ContainerInfo>,
    /// Aggregate thegn container footprint (owned counts + engine disk usage)
    /// for the monitor's Containers-tab header. Populated only while a
    /// per-container-stats surface is visible (the visibility gate); `None`
    /// until then. Kept across cheap ticks — see the container_rx drain.
    pub container_footprint: Option<thegn_core::sandbox_manage::ContainerFootprint>,
    /// The agent-dispatch roster behind the monitor's Pipeline tab, plus the
    /// configured stage order it groups by. LOOP-owned like `containers`:
    /// sampled off-loop only while the board is live (`RefreshKind::Dispatches`)
    /// and carried across the model swap, so hydration never blanks it.
    pub dispatches: crate::monitor_pipeline::DispatchRoster,
    /// Health of the active worktree's container (updated on the container refresh tick).
    pub container_health: ContainerHealth,
    /// Recent audit events for the active worktree's container (last 10, newest first).
    pub container_events: Vec<thegn_core::models::ContainerEvent>,
    /// Unified per-worktree activity timeline: the sandbox audit log and the
    /// LLM-proxy request/spend log merged and time-sorted (newest first). The
    /// cross-backend "what is this worktree doing" view, rendered in System →
    /// sandbox. Built off-loop by [`merge_timeline`](thegn_core::models::merge_timeline).
    pub timeline: Vec<thegn_core::models::TimelineEvent>,
    /// Names of orphan containers removed at startup GC (shown once in System panel).
    pub startup_orphans_removed: Vec<String>,
    /// Top-level app-tab chip labels in masthead order: `work` first, then the
    /// embedded apps (`chat`, …). Empty hides the strip entirely.
    pub app_tabs: Vec<String>,
    /// Index of the active app tab in [`Self::app_tabs`] (0 = `work`).
    pub active_app: usize,
    /// Whether the outer terminal can report `Ctrl+<digit>` distinctly, from
    /// the startup probe. `None` = unknown (no probe / silent terminal) —
    /// which MUST be treated as "assume it works". See THE-70.
    ///
    /// Session-constant: set once from the probe and re-stamped across model
    /// swaps (like [`Self::plugin_segments`]), never recomputed per frame.
    pub ctrl_digits_reportable: Option<bool>,
    /// Ordered launch steps shown in the loading screen while the first pane
    /// is spawning. Empty = no loading screen. Cleared once a live pane exists.
    pub load_steps: Vec<LoadStep>,
    /// Context shown beneath the loading steps: `(key, value)` facts about where
    /// the pane is coming up — env, placement, provider/sandbox, connect, workdir.
    /// Empty hides the block. Only meaningful while [`Self::load_steps`] is set.
    pub load_context: Vec<(String, String)>,
    /// Plugin statusbar segments as `(label, view)`, in stable plugin order.
    /// LOOP-owned (synced from `handlers::plugins`, not hydration — like
    /// `shares`/`forwards` it is re-stamped after every model swap); rendered
    /// by [`draw_statusbar`] in the gap between the left and right clusters.
    pub plugin_segments: Vec<(String, thegn_core::plugin_api::View)>,
    /// Loop-owned state for the removable bottom drawer presence widget.
    /// Keeping this small snapshot on the renderer model makes the widget
    /// pure and keeps it stable across hydration model swaps.
    pub drawer_bar: DrawerBarState,
}

/// The renderer-facing state of the bottom drawer indicator.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DrawerBarState {
    /// Whether a drawer pane is currently visible.
    pub open: bool,
    /// Stable occupant label (`files` or the configured tool name).
    pub occupant: String,
    /// Number of valid occupants available in the active scope, including
    /// the built-in files occupant.
    pub occupant_count: usize,
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
    /// What the step *is* — a stable identity that survives label refinement
    /// (`"container"` → `"container (podman)"`), used by the loop-side tracker
    /// to carry timing across whole-Vec replacement and by the splash to pick
    /// a slow-step hint. Producers that predate kinds default to `Other`.
    pub kind: StepKind,
    /// When the step last became `Active`. Stamped by the loop-side
    /// `LoadingTracker` at drain time (never by producers), frozen on the
    /// transition out of `Active`, so the splash can render per-step elapsed
    /// time. Changes only on state transitions — an idle wake still compares
    /// equal, keeping the render-plan `Skip` contract.
    pub started_at: Option<std::time::Instant>,
    /// How long the step ran, measured by the tracker at the `Active` →
    /// `Done`/`Failed` transition. The splash's elapsed column shows this for
    /// finished steps (a frozen `started_at` alone would keep counting).
    pub took: Option<std::time::Duration>,
    /// Byte progress `(done, total)` for a network-bound step (image pull);
    /// `total` is `None` until the runtime reports sizes. Drawn as a bar
    /// under the active step.
    pub progress: Option<(u64, Option<u64>)>,
}

/// The step-identity vocabulary for [`LoadStep::kind`] — what phase of a
/// bring-up a step represents, independent of its display label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepKind {
    /// Backend/chain resolution ("sandbox").
    Resolve,
    /// Remote transport connect (ssh/mosh/provider).
    Connect,
    /// Image probe/pull.
    Image,
    /// Dockerfile/devcontainer image build.
    Build,
    /// Container / namespace create.
    Create,
    /// VPN sidecar bring-up.
    Vpn,
    /// Remote filesystem projection (sshfs/sync).
    Mount,
    /// Environment wrap warm-up (direnv/devenv/devShell).
    Env,
    /// A provider provisioning step (clone/nix/setup/… streams).
    Provision,
    /// The final login-shell attach (the `is_shell_wait` gate).
    Shell,
    /// Unclassified (legacy producers).
    #[default]
    Other,
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
    fn new(label: impl Into<String>, state: StepState) -> Self {
        Self {
            label: label.into(),
            state,
            detail: None,
            kind: StepKind::Other,
            started_at: None,
            took: None,
            progress: None,
        }
    }
    pub fn pending(label: impl Into<String>) -> Self {
        Self::new(label, StepState::Pending)
    }
    pub fn active(label: impl Into<String>) -> Self {
        Self::new(label, StepState::Active)
    }
    pub fn done(label: impl Into<String>) -> Self {
        Self::new(label, StepState::Done)
    }
    pub fn failed(label: impl Into<String>) -> Self {
        Self::new(label, StepState::Failed)
    }
    /// Attach a dim sub-line (status / error) under the step.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        self.detail = (!d.trim().is_empty()).then_some(d);
        self
    }
    /// Tag the step's stable identity (see [`StepKind`]).
    pub fn with_kind(mut self, kind: StepKind) -> Self {
        self.kind = kind;
        self
    }
}

/// Action ids whose chords the chrome prints inline, resolved into
/// [`FrameModel::chord_hints`] each frame. Add an id here when a draw site
/// needs to name a key, rather than writing the chord as a literal.
pub const INLINE_CHORD_IDS: &[&str] = &["help", "palette", "toggle-key-lock"];

impl FrameModel {
    /// The live chord for an action id, for chrome that names a key inline.
    /// Falls back to `id` so a draw site always has something to print (and a
    /// missing binding reads as the action's name, never as a wrong chord).
    pub fn chord<'a>(&'a self, id: &'a str) -> &'a str {
        self.chord_hints.get(id).map(String::as_str).unwrap_or(id)
    }

    /// Whether `[monitor] processes` leaves process sampling on. See
    /// [`Self::procs_disabled`] for why the stored field is inverted.
    pub fn procs_enabled(&self) -> bool {
        !self.procs_disabled
    }

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
        .map(|c| UnicodeWidthStr::width(format!(" {} {} ", c.glyph, c.label).as_str()))
        .sum();
    end.saturating_sub(total).max(strip.x)
}

/// The tab chips' `(x, width, tab index)` spans inside the center tab bar —
/// the single source of truth consumed by BOTH [`draw_center_tabs`]
/// (placement) and [`center_tab_hit`] (mouse), so they can never drift apart.
/// The linked-issue badge painted right after the worktree leaf (` ◈123`), or
/// `None` when no issue is linked. One source for the painter and for
/// [`strip_chip_spans`], which must skip its cells — the first tab chip used to
/// be placed over it, leaving a bare `◈` with the number never visible.
fn issue_badge(model: &FrameModel) -> Option<String> {
    let issue_id = model.panel.tracker_links.first()?;
    let issue = model
        .panel
        .tracker_issues
        .iter()
        .find(|i| &i.id == issue_id)?;
    Some(format!(" \u{25c8}{}", issue.number))
}

/// The column at which the tab chips stop: the pin strip's start, less the env
/// cluster drawn before it. One source for [`strip_chip_spans`] (which stops
/// placing there) and [`center_tab_hit`] (which clamps its widened hit span
/// there), so a widened chip can never claim a pin's or the cluster's cell.
fn strip_chip_end(model: &FrameModel, strip: Rect) -> usize {
    pin_chips_start(model, strip)
        .saturating_sub(crate::tabbar_env::env_cluster_width(model))
        .max(strip.x)
}

fn strip_chip_spans(model: &FrameModel, strip: Rect) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    if strip.rows == 0 || strip.cols == 0 {
        return spans;
    }
    let end = strip_chip_end(model, strip);
    let mut x = strip.x + 1;
    if let Some((ws, leaf)) = worktree_parts(model) {
        if !ws.is_empty() {
            x += UnicodeWidthStr::width(ws.as_str()) + 3; // "WS ▸ "
        }
        x += UnicodeWidthStr::width(leaf.as_str()); // "leaf"
        if let Some(badge) = issue_badge(model) {
            x += UnicodeWidthStr::width(badge.as_str());
        }
        x += 2; // gap
    }
    for (i, title) in model.tabs.iter().enumerate() {
        let w = UnicodeWidthStr::width(title.as_str()) + 2; // " {title} "
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
///
/// The hit span is one column wider than the painted chip — `[sx, sx + w + 1)`
/// — because chips are laid out with a single spacing column after each one
/// (`x += w + 1`). Claiming it leaves no dead cell between chips: the next chip
/// starts exactly at `sx + w + 1`, so spans stay non-overlapping and the gap
/// resolves to the chip on its left. Placement is untouched; the widening is
/// clamped at [`strip_chip_end`] so it never reaches the env cluster or a pin.
pub fn center_tab_hit(model: &FrameModel, strip: Rect, x: usize) -> Option<usize> {
    let end = strip_chip_end(model, strip);
    strip_chip_spans(model, strip)
        .into_iter()
        .find(|(sx, w, _)| x >= *sx && x < (sx + w + 1).min(end))
        .map(|(_, _, i)| i)
}

/// Brand slot widths for the masthead text logo.
/// " ◆ thegn v0.0.0 " — glyph + name + version…
pub(crate) const BRAND_FULL_COLS: usize = 17;
/// …or just " ◆ thegn " on narrower screens…
pub(crate) const BRAND_COMPACT_COLS: usize = 10;
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
pub(crate) fn cluster_width(parts: &[(String, usize)], kept: &[usize]) -> usize {
    if kept.is_empty() {
        return 0;
    }
    kept.iter().map(|&i| parts[i].1).sum::<usize>() + 3 * (kept.len() - 1) + 1
}

/// Drop right-cluster widgets in priority order until the cluster fits `avail`
/// columns — softest stats shed first, leaving cpu/mem/net/battery longest —
/// and then, if it *still* does not fit, from the right end inward, so the
/// cluster can always be emptied rather than eat the brand + active chip it
/// was budgeted around. Returns the surviving indices in display order.
///
/// `date` is the softest: the `clock` beside it carries the same information.
/// `weather` is next — the user opted into it, so it outlives `uptime`/`load`/
/// `freq`, but a datum that changes twice an hour is still not `cpu`.
pub(crate) fn fit_stats_cluster(parts: &[(String, usize)], avail: usize) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..parts.len()).collect();
    for victim in [
        "date", "weather", "uptime", "load", "freq", "swap", "temp", "disk", "gpu", "battery",
        "net", "clock", "mem", "cpu",
    ] {
        if cluster_width(parts, &kept) <= avail {
            break;
        }
        kept.retain(|&i| parts[i].0 != victim);
    }
    while !kept.is_empty() && cluster_width(parts, &kept) > avail {
        kept.pop();
    }
    kept
}

/// The single-row masthead: a regular-font text brand + app-tab chips +
/// `top_left` breadcrumb on the left and the `[bars]`-configured stats cluster
/// on the right. Built on the same seg / [`crate::seg::Line::split`] machinery
/// as the statusbar (see [`crate::masthead::masthead_layout`]), so it degrades
/// identically at narrow widths — display-width measurement, atomic-unit left
/// fit, a ghost `…` on the breadcrumb, and the stats cluster shedding by
/// priority — with nothing ever clipping mid-glyph or overlapping. (The pixel
/// wordmark survives on the empty-state splash.)
pub fn draw_masthead(
    surface: &mut Surface,
    layout: &crate::layout::ChromeLayout,
    model: &FrameModel,
) {
    use crate::seg::{Line, Tok, draw_line};
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

    // Focused selection on the right stats cluster: bright focus fg over a
    // focus-tinted pill, matching the app-chips' active look.
    let sel = model.masthead_focused.then(|| {
        let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.28));
        (model.masthead_sel, pill, col(S::Focus))
    });
    let lay = crate::masthead::masthead_layout(model, rect.cols, sel);
    let row = layout.masthead_stats_row();
    draw_line(
        surface,
        row.x,
        row.y,
        row.cols,
        &Line::split(lay.left, lay.right),
        Tok::Slot(bar_bg),
    );
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

    draw_pin_chips(surface, strip, end, model);
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
            x += UnicodeWidthStr::width(ws.as_str());
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
        x += UnicodeWidthStr::width(leaf.as_str());
        // Issue badge: the first linked issue's number next to the active
        // worktree name. `strip_chip_spans` reserves these cells.
        if let Some(badge) = issue_badge(model) {
            let avail = chips_end.saturating_sub(x);
            if avail >= UnicodeWidthStr::width(badge.as_str()) {
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

/// Build the pin-chip strip as a chrome **element**: one pass produces the
/// paintable row and its per-chip hit spans (`Alt-N` summon), so the hit table
/// can never drift from what was painted. Shared by [`draw_pin_chips`]
/// (painting) and [`pin_chip_hit`] (mouse) — this is the element contract's
/// builder rule applied to the strip that was previously click-dead.
///
/// Chips read ` glyph label `, right-aligned; running pins in the accent,
/// stopped/failed dim — the exact look `draw_text` produced, now expressed in
/// tokens so it degrades through the one chokepoint.
fn build_pin_strip(model: &FrameModel, content: Rect) -> crate::element::ChipRow {
    use crate::element::{ChipRow, ElementAction};
    use crate::seg::{Tok, seg};
    let content_end = content.x + content.cols;
    let start = pin_chips_start(model, content);
    let accent = theme_color(model.accent_or_default());
    let running = crate::pins::PinHealth::Running.glyph();
    let mut row = ChipRow::new(start, content.y);
    for pin in &model.pins {
        if start + row.width() >= content_end {
            break;
        }
        // Each chip reads " <glyph> <label> " (the leading index is implicit
        // Alt-N). Running pins read in the accent; stopped/failed read dim.
        let text = format!(" {} {} ", pin.glyph, pin.label);
        let fg = if pin.glyph == running {
            Tok::Attr(accent)
        } else {
            Tok::Slot(S::Dim)
        };
        let chip = seg(fg, text).bg(Tok::Slot(S::Panel));
        row.push(chip, ElementAction::SummonPin(pin.index));
    }
    row
}

/// Render the pin chips right-aligned in the tab-strip area, through the element
/// contract: [`build_pin_strip`] produces the row (and its hit spans) in one
/// pass, and this paints the row into the same cells the spans cover.
fn draw_pin_chips(surface: &mut Surface, content: Rect, content_end: usize, model: &FrameModel) {
    if model.pins.is_empty() {
        return;
    }
    let row = build_pin_strip(model, content);
    let start = pin_chips_start(model, content);
    let width = content_end.saturating_sub(start);
    crate::seg::draw_line(
        surface,
        start,
        content.y,
        width,
        &row.line(),
        crate::seg::Tok::Slot(S::Panel),
    );
}

/// Which pin's 1-based `Alt-N` index sits at column `x` of the tab strip (mouse
/// hit-test), or `None`. Reads the hit spans emitted by [`build_pin_strip`], the
/// same build that paints — so the pins, previously click-dead, resolve a click
/// to exactly the chip drawn at that cell.
pub fn pin_chip_hit(model: &FrameModel, strip: Rect, x: usize) -> Option<usize> {
    if model.pins.is_empty() || strip.rows == 0 || strip.cols == 0 {
        return None;
    }
    let row = build_pin_strip(model, strip);
    match crate::element::hit_at(row.hits(), x, strip.y) {
        Some(crate::element::ElementAction::SummonPin(i)) => Some(*i),
        _ => None,
    }
}

/// A resolved masthead widget: its text plus the color it earned (stats turn
/// amber/red as they cross pressure thresholds; quiet otherwise).
pub struct MastheadWidget {
    pub(crate) text: String,
    pub(crate) fg: ColorAttribute,
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
    /// The bottom-left `?` chip: the one always-visible, clickable pointer at
    /// the help system. Opens the F1 overlay rather than a detail popup.
    Help,
}

/// The statusbar's always-on badges, in render order. Each maps to one of the
/// imperative badge blocks in [`draw_statusbar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarBadge {
    Notifications,
    /// Needs-you rollup: worktrees whose attention tier is T0–T2.
    Attention,
    Ci,
    /// Legacy queue badge id retained for detail/compatibility. The default
    /// statusbar now emits the configured `Widget("mq")` instead.
    #[allow(dead_code)]
    MergeQueue,
    /// PR queue: pull requests being shepherded on a forge.
    PrQueue,
    /// Focused pane is daemon-backed (survives quitting the UI).
    Persist,
    DiskWarn,
    Ingress,
    Media,
    /// AI-account rate-limit usage (`[usage]`).
    Usage,
    /// Network is offline — remote refreshes/MCPs paused.
    Network,
    Zoom,
    Maximized,
    Lock,
    Sync,
}

impl BarItemId {
    /// Whether activating this item (Enter / click) opens a detail view.
    /// `Help` opts out: it opens the help overlay instead of a bar popup.
    pub fn has_detail(&self) -> bool {
        !matches!(self, BarItemId::Help)
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

/// Rising-threshold level straight from a configured
/// [`AlertRule`](thegn_core::resource_alert::AlertRule) — the mirror
/// of [`free_level`]'s falling one.
///
/// Takes the rule rather than hardcoding numbers so the widget colour and the
/// alert cannot disagree. `[stats]` already promises exactly that for
/// `disk_free`/`battery` ("each threshold has exactly one place to be set"); the
/// promise simply had no implementation for the rising metrics. A rule at 0 is
/// disabled, and disabled must read as Normal, not as "0 exceeded".
fn rule_level(value: f32, rule: &thegn_core::resource_alert::AlertRule) -> Level {
    if !value.is_finite() {
        return Level::Normal;
    }
    if rule.critical > 0.0 && value >= rule.critical {
        Level::Crit
    } else if rule.warn > 0.0 && value >= rule.warn {
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

/// The time the clock/date widgets show — real, unless the e2e freeze pins it.
fn wall_clock() -> chrono::DateTime<chrono::Local> {
    if crate::e2e_freeze::active() {
        crate::e2e_freeze::now()
    } else {
        chrono::Local::now()
    }
}

/// Resolve a masthead widget id to its display text + color; `None` hides the
/// widget (no data yet, GPU absent, unknown id).
pub(crate) fn masthead_widget(id: &str, model: &FrameModel) -> Option<MastheadWidget> {
    let s = &model.stats;
    let ic = &model.stats_icons;
    let w = |text: String, fg: ColorAttribute| MastheadWidget { text, fg };
    match id {
        "brand" => Some(w(
            format!("thegn {}", crate::e2e_freeze::version_label()),
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
        // Coloured per CORE against the configured rule, not left dim: load is
        // the only widget that shows OVERSUBSCRIPTION, and an always-dim one is
        // why a box at 3.25x per core still looked calm on the bar.
        "load" => s.load_avg.map(|(one, _, _)| {
            let per_core = one / s.cpu_cores.len().max(1) as f32;
            let rule = ic
                .effective_alerts()
                .rule(thegn_core::resource_alert::AlertMetric::Load);
            w(
                format!("{} {one:.2}", ic.load_icon),
                level_color(rule_level(per_core, &rule)),
            )
        }),
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
                    thegn_metrics::fmt_rate(rx),
                    crate::caps::active_glyphs().arrow_up,
                    thegn_metrics::fmt_rate(tx)
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
            wall_clock().format(&model.bars.date_format).to_string(),
            col(S::Dim),
        )),
        "clock" => Some(w(
            wall_clock().format(&model.bars.clock_format).to_string(),
            col(S::Dim),
        )),
        // `[weather]`. Absent — not blank — when disabled, before the first
        // reading, or once the reading is hard-expired: a weather widget
        // showing last Tuesday's sun is worse than no widget at all.
        "weather" => {
            use thegn_core::weather::Freshness;
            let snap = model.weather.as_ref()?;
            let cfg = &model.weather_cfg;
            let glyph = thegn_core::weather::sky_glyph(snap.sky, crate::caps::active_glyphs());
            let temp = thegn_core::weather::fmt_temp(snap.temp, snap.units);
            // `Sky::Unknown` has no glyph; the temperature stands alone rather
            // than behind a leading space.
            let text = if glyph.is_empty() {
                temp
            } else {
                format!("{glyph} {temp}")
            };
            match thegn_core::weather::freshness(
                snap.fetched_at,
                wall_clock().timestamp(),
                cfg.stale_after_secs,
                cfg.hard_expiry_secs,
            ) {
                Freshness::Expired => None,
                Freshness::Fresh => Some(w(text, col(S::Dim))),
                // Dimmer, not coloured: staleness is a quiet caveat, not an
                // alert — nobody needs a red badge because it is three hours
                // since the last forecast.
                Freshness::Stale => Some(w(text, col(S::Ghost))),
            }
        }
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
        "drawer" => {
            let glyph = crate::caps::glyph(crate::caps::Glyph::Folder);
            let count = if model.drawer_bar.occupant_count > 1 {
                format!(" ({})", model.drawer_bar.occupant_count)
            } else {
                String::new()
            };
            let text = if model.drawer_bar.open {
                let label = if model.drawer_bar.occupant.is_empty() {
                    "files"
                } else {
                    model.drawer_bar.occupant.as_str()
                };
                format!("{glyph} {label}{count}")
            } else {
                format!("{glyph} drawer{count}")
            };
            Some(w(
                text,
                if model.drawer_bar.open {
                    col(S::Accent)
                } else {
                    col(S::Dim)
                },
            ))
        }
        "loc" => model
            .loc
            .as_ref()
            .map(|r| w(crate::i18n_surface::loc(&r.compact_total()), col(S::Dim))),
        // Active worktree's disk usage (size of its checkout incl. target/),
        // from the off-loop scan; sits next to LOC. Hidden until first scanned.
        "disk" => model
            .active_worktree_disk
            .map(|b| w(thegn_core::disk::human(b), col(S::Dim))),
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

/// The statusbar's ordered navigable right-cluster items — the `bottom_right`
/// config widgets followed by the always-on badges — each as its stable id plus
/// the segments that render it (WITHOUT the inter-item separator, which the
/// layout adds). The single source of truth shared by [`draw_statusbar`]
/// (placement + highlight), navigation (item count), and
/// [`statusbar_item_spans`] (mouse + popup anchoring), so they can never drift.
pub fn statusbar_items(model: &FrameModel) -> Vec<(BarItemId, Vec<crate::seg::Seg>)> {
    use crate::seg::{Seg, Tok, seg};
    let mut items: Vec<(BarItemId, Vec<Seg>)> = Vec::new();

    // Config widgets (PR / tests / LOC / disk / transient status). `mq` is a
    // compact, data-backed widget and is rendered by the badge module rather
    // than by the text-only widget resolver below.
    for id in &model.bars.bottom_right {
        if id == "mq" {
            crate::statusbar_badges::push_mq_widget(model, &mut items);
            continue;
        }
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
                Tok::Hue(thegn_core::theme::Hue::Amber),
                format!(" \u{25cf} dnd{suffix} "),
            )],
        ));
    } else if !model.notify_mode.is_empty() {
        items.push((
            BarItemId::Badge(BarBadge::Notifications),
            vec![Seg::chip(
                Tok::Hue(thegn_core::theme::Hue::Teal),
                format!(" \u{25c9} {} ", model.notify_mode),
            )],
        ));
    }

    // The attention chip (`✋ N` / quiet `✉ N`) is the ONE inbox/needs-you
    // signal — see `statusbar_badges::push_attention_badge`. (A separate `⚑`
    // alert-count chip used to sit here and double-counted the same events.)
    // Needs-you / CI rollup chips live in `statusbar_badges.rs`
    // (extracted from this ratchet-pinned file).
    crate::statusbar_badges::push_attention_badge(model, &mut items);
    crate::statusbar_badges::push_network_chip(model, &mut items);
    crate::statusbar_badges::push_ci_badge(model, &mut items);
    crate::statusbar_badges::push_prq_badge(model, &mut items);
    crate::statusbar_badges::push_usage_badge(model, &mut items);
    // Low-free-space badge: trips when the worktrees' filesystem drops to/below
    // `[stats].disk_free_warn` free — amber at the warn line, red at/below
    // `disk_free_critical`. The badge selects into a detailed modal (free/used/
    // total bytes + worktree usage). Silent above the warn line (clean is quiet)
    // and until the stats sampler has produced a reading.
    if let Some(free) = model.stats.disk_free_pct {
        let ic = &model.stats_icons;
        let hue = match free_level(free, ic.disk_free_warn, ic.disk_free_critical) {
            Level::Crit => Some(thegn_core::theme::Hue::Red),
            Level::Warn => Some(thegn_core::theme::Hue::Amber),
            Level::Normal => None,
        };
        if let Some(hue) = hue {
            items.push((
                BarItemId::Badge(BarBadge::DiskWarn),
                vec![Seg::chip(
                    Tok::Hue(hue),
                    format!(
                        " {} {} ",
                        ic.disk_icon,
                        crate::i18n_surface::disk_free(free)
                    ),
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
                thegn_core::theme::Hue::Amber
            } else {
                thegn_core::theme::Hue::Teal
            };
            items.push((
                BarItemId::Badge(BarBadge::Ingress),
                vec![Seg::chip(Tok::Hue(hue), label)],
            ));
        } else if failed > 0 {
            items.push((
                BarItemId::Badge(BarBadge::Ingress),
                vec![Seg::chip(
                    Tok::Hue(thegn_core::theme::Hue::Amber),
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
        use thegn_core::media::PlaybackState;
        let hue = match m.state {
            PlaybackState::Playing => thegn_core::theme::Hue::Green,
            _ => thegn_core::theme::Hue::Blue,
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
    if model.zoomed {
        let label = crate::i18n_surface::status(crate::i18n_surface::StatusText::Zoom);
        items.push((
            BarItemId::Badge(BarBadge::Zoom),
            vec![Seg::chip(
                Tok::Hue(thegn_core::theme::Hue::Purple),
                format!(" \u{26f6} {label} "),
            )],
        ));
    }
    // Maximize-in-chrome (mutually exclusive with `zoomed`): a framed square
    // reads as "pane filling its region", distinct from the fullscreen chip.
    if model.maximized {
        let label = crate::i18n_surface::status(crate::i18n_surface::StatusText::Maximized);
        items.push((
            BarItemId::Badge(BarBadge::Maximized),
            vec![Seg::chip(
                Tok::Hue(thegn_core::theme::Hue::Teal),
                format!(" \u{25a3} {label} "),
            )],
        ));
    }
    if model.key_locked {
        let label = crate::i18n_surface::status(crate::i18n_surface::StatusText::Locked);
        items.push((
            BarItemId::Badge(BarBadge::Lock),
            vec![Seg::chip(
                Tok::Hue(thegn_core::theme::Hue::Amber),
                format!(" \u{2301} {label} "),
            )],
        ));
    }
    if model.sync_panes {
        let label = crate::i18n_surface::status(crate::i18n_surface::StatusText::Sync);
        items.push((
            BarItemId::Badge(BarBadge::Sync),
            vec![Seg::chip(
                Tok::Hue(thegn_core::theme::Hue::Red),
                format!(" \u{29c9} {label} "),
            )],
        ));
    }
    // The always-on daemon/status indicator is pushed LAST so it renders at the
    // far right of the bottom bar (the right cluster is right-aligned in order).
    crate::statusbar_badges::push_daemon_chip(model, &mut items);
    items
}

/// Recolor an item's segments as the focused-selection block: bright focus
/// foreground over a focus-tinted pill, matching the masthead app-chips' active
/// look ("reads like a selected tab"). Width is unchanged, so spans are stable.
pub(crate) fn highlight_segs(
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

/// Cells the right cluster must leave for the left cluster's `?` chip (its
/// leading pad + ` ? ` + the `Line::split` gutter) when the help chip is
/// configured — so shedding never paints the badges over the one always-on
/// pointer at the help system.
fn statusbar_left_reserve(model: &FrameModel) -> usize {
    if model
        .bars
        .bottom_left
        .iter()
        .any(|id| id == crate::statusbar_left::HELP_ID)
    {
        5
    } else {
        0
    }
}

/// [`statusbar_items`] after priority shedding for a bar `cols` wide — **the**
/// list every consumer uses (painter, spans, ←/→ count, Enter), so what is
/// painted, what is clickable and what the cursor can reach are one set.
pub fn statusbar_items_fitted(
    model: &FrameModel,
    cols: usize,
) -> Vec<(BarItemId, Vec<crate::seg::Seg>)> {
    let avail = cols.saturating_sub(statusbar_left_reserve(model));
    crate::statusbar_fit::fit(statusbar_items(model), avail)
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
    let items = statusbar_items_fitted(model, rect.cols);
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

/// Cells the left cluster gets once the right cluster wins its space. Mirrors
/// `Line::split`'s math, so the keyhint strip can be trimmed at whole-binding
/// boundaries instead of being cut mid-chord by the generic ellipsis. Shared
/// with `statusbar_left::left_item_spans` so paint and hit-test agree.
pub fn statusbar_left_budget(model: &FrameModel, rect: Rect) -> usize {
    use crate::seg::seg_width;
    let (r, _) = statusbar_right_layout(&statusbar_items_fitted(model, rect.cols), None);
    let rl = seg_width(&r);
    rect.cols.saturating_sub(rl + usize::from(rl > 0))
}

/// The bottom widget bar: the `?` help chip + mode chip + `[bars] bottom_left`
/// (context keybind hints as key chips + dim labels) left-aligned,
/// `bottom_right` (PR / LOC / transient status) right-aligned with `│` rules,
/// and the zoom/lock badges as inverse chips always outermost-right.
pub fn draw_statusbar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    use crate::seg::{Line, Tok, draw_line};
    if rect.rows == 0 {
        return;
    }

    // Right side: per-widget colors with `│` rules then the badges, built from
    // the shared enumerator so navigation/highlight/hit-test stay in lock-step.
    // The right cluster wins space — the left fits in what's left.
    let items = statusbar_items_fitted(model, rect.cols);
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
    let (l, _spans) = crate::statusbar_left::left_layout(model, statusbar_left_budget(model, rect));

    // Plugin segments render into the free gap between the two clusters, so
    // remember the cluster widths before `Line::split` consumes the segs.
    let left_w = crate::seg::seg_width(&l);
    let right_w = crate::seg::seg_width(&r);

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

    // Plugin statusbar segments: painted after the bar so they overlay only
    // the pad gap between the left cluster and the right-aligned cluster,
    // one space apart. A segment that would not fit whole is skipped — narrow
    // widths degrade by dropping segments, never by clipping mid-segment.
    if !model.plugin_segments.is_empty() {
        let mut x = rect.x + left_w + 1;
        let end = (rect.x + rect.cols).saturating_sub(right_w + usize::from(right_w > 0));
        for (_label, view) in &model.plugin_segments {
            let w = view.text_content().chars().count();
            if w == 0 || x + w > end {
                continue;
            }
            draw_plugin_view(
                surface,
                Rect {
                    x,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
                view,
                model.accent_or_default(),
            );
            x += w + 1;
        }
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

    // Navigation-hints footer: the same "reveal on focus, only in blank tail
    // space" language as the sidebar (`draw_sidebar_hints`), but sourced from
    // the live `model.keyhints` — the config-resolved, context-sensitive
    // hotkeys the statusbar already shows for the focused panel — so it tracks
    // the real bindings (and any remaps) instead of a static table. Painting
    // only over rows `build_panel` didn't produce means it never overlaps panel
    // content and needs no change to `panel_hits`.
    if model.panel_focused {
        let pairs: Vec<(String, String)> = model.keyhints.iter().take(6).cloned().collect();
        let hint_h = pairs.len() + 1; // rule/title + one row per tip
        let avail = rect.rows.saturating_sub(frame.rows.len());
        // Require a one-row gap between the last panel row and the footer.
        if !pairs.is_empty() && avail > hint_h {
            draw_panel_hints(
                surface,
                Rect {
                    x: rect.x,
                    y: rect.y + rect.rows - hint_h,
                    cols: rect.cols,
                    rows: hint_h,
                },
                &pairs,
            );
        }
    }
}

/// Paint the panel's navigation-hints footer: a rule + " NAVIGATE " title over a
/// column of dim chord / label pairs. Mirrors the sidebar's `draw_sidebar_hints`
/// so the two focus footers read identically; the pairs are the live
/// `model.keyhints`, so the content is dynamic (see [`draw_panel`]).
fn draw_panel_hints(surface: &mut Surface, rect: Rect, pairs: &[(String, String)]) {
    if rect.rows < 2 || rect.cols == 0 {
        return;
    }
    let line = crate::caps::active_glyphs().box_h.repeat(rect.cols);
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
        " NAVIGATE ",
        col(S::Dim),
        col(S::Panel),
        rect.cols.saturating_sub(1),
    );

    let chord_w = pairs
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    let max_y = rect.y + rect.rows;
    let right = rect.x + rect.cols;
    for (i, (chord, label)) in pairs.iter().enumerate() {
        let y = rect.y + 1 + i;
        if y >= max_y {
            break;
        }
        draw_text(
            surface,
            rect.x + 1,
            y,
            chord,
            col(S::Faint),
            col(S::Panel),
            rect.cols.saturating_sub(1),
        );
        let lx = rect.x + 1 + chord_w + 1;
        if lx < right {
            draw_text(
                surface,
                lx,
                y,
                label,
                col(S::Ghost),
                col(S::Panel),
                right - lx,
            );
        }
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
        crate::sidebar_view::draw_sidebar(surface, sb, model);
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

/// Whether the center band shows the fullscreen loading splash instead of pane
/// content, given the laid-out center leaves (`frames`, from
/// [`crate::center::CenterTree::layout_framed`]) and the model.
///
/// The single source of truth shared by the full-frame path ([`render_panes`])
/// and the incremental pane-output path (the `RenderPlan::Incremental` arm in
/// `run.rs`), so the two agree on what the center shows for a given state.
/// Before this was factored out, only the full path consulted it — so while a
/// live pane streamed output under a still-up splash, incremental pane-output
/// frames painted the pane while full frames painted the splash, flashing the
/// loading screen in and out.
///
/// - `!any_live`: no visible leaf has a live emulator (fresh launch before the
///   first pane materializes, or every pane died) — the splash replaces what
///   would otherwise be a black hole, and disappears the frame a pane shows up.
/// - `!load_steps.is_empty() && frames.len() == 1`: pane-launch steps are in
///   progress. Only a single-leaf center may take the *fullscreen* splash — a
///   split (>= 2 leaves) always has an existing pane to keep rendering, so
///   `load_steps` must never black it out.
pub fn center_shows_splash(
    frames: &[(crate::center::PaneId, Rect, Rect)],
    model: &FrameModel,
    is_live: impl Fn(crate::center::PaneId) -> bool,
) -> bool {
    let any_live = frames.iter().any(|(id, _, _)| is_live(*id));
    !any_live || (!model.load_steps.is_empty() && frames.len() == 1)
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
    let show_splash = center_shows_splash(&frames, model, |id| lookup(id).is_some());
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
        // Drag-rearrange affordance: light up the pane a drop would land on
        // (whole ring = swap, one edge = re-anchor). Painted last so it reads on
        // top; colours come from the theme role, not a literal.
        if let Some(viz) = model.pane_drag
            && let Some((_, frame, _)) = frames.iter().find(|(id, _, _)| *id == viz.target)
        {
            let kind = match viz.kind {
                crate::pane_drag::DropKind::Swap => crate::borders::DropHighlight::Swap,
                crate::pane_drag::DropKind::Anchor(side) => {
                    crate::borders::DropHighlight::Side(side)
                }
            };
            crate::borders::draw_drop_highlight(
                surface,
                *frame,
                kind,
                col(S::Accent),
                col(S::Panel),
            );
        }
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
