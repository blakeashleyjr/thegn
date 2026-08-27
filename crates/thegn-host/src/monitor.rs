//! The tabbed system-monitor modal.
//!
//! Lifecycle mirrors [`crate::pr_view::PrView`]: an `Option<MonitorOverlay>`
//! slot on the loop, fed `handle_key`, painted late over the composed frame,
//! dismissed on `Esc`/`q`. It draws [`crate::sections::Section`] stacks, the
//! same vocabulary the bar-item popups use.
//!
//! # Why this isn't a `DetailOverlay`
//!
//! [`crate::detail::DetailOverlay`] measures its scroll against the whole box,
//! and its key handler already forks every navigation key on whether the
//! content is an actionable list. A tab bar plus a footer means two more
//! reserved rows, and the monitor needs `g`/`s`/`[`/`]`/`Space` to stay global
//! while the Processes tab is simultaneously a sortable list. Threading both
//! through a function six live surfaces share is how the popups' scroll clamp
//! would regress — the failure mode `detail::sections`' doc comment exists to
//! warn about.
//!
//! # Live, but not a wake source
//!
//! [`MonitorOverlay::refresh`] rides the loop's existing stats drain, so an open
//! monitor adds no timer and no thread. Pausing freezes the *view* only: the
//! history rings keep filling underneath, so resuming shows a continuous
//! timeline rather than a gap, and no snapshot is ever cloned.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::{FrameModel, S};
use crate::compositor::Rect;
use crate::detail::StatusCtx;
use crate::layer::{self, Anchor, LayerSpec};
use crate::sections::{self, GraphStyle, Section};
use crate::seg::{Line, Seg, Tok, seg};
use crate::telemetry::{ScaleMode, Window};
use thegn_metrics::StatsSnapshot;

mod build;
pub(crate) mod procs_view;
pub(crate) mod state;

pub(crate) use state::MonitorPrefs;

/// The view toggles the Processes tab's row list depends on. Bundled so the pure
/// [`procs_view::rows`] builder and the overlay share one input shape and cannot
/// disagree about which rows are shown.
#[derive(Debug, Clone)]
pub(crate) struct ProcSnapshotView {
    pub sort: ProcSort,
    pub desc: bool,
    pub filter: String,
    pub tree: bool,
}

/// A destructive action awaiting a `y`/`n` confirmation in the footer. Both
/// rungs name what they will do so a pane-owned build is recognizably thegn's
/// own before anything is signalled.
#[derive(Debug, Clone)]
enum Confirm {
    /// Deliver `stage` to a process.
    Signal {
        pid: u32,
        label: String,
        stage: crate::platform::ProcSignal,
    },
    /// Reclaim a worktree's `target/`.
    Clean {
        path: std::path::PathBuf,
        label: String,
    },
}

/// An action the loop must perform on the overlay's behalf because it needs
/// resources the overlay doesn't hold (a background thread + the DB, a
/// lifecycle subprocess, or the pane/session tables). The signal action is NOT
/// here — it is a self-contained syscall the overlay makes directly, which is
/// what keeps it TUI-only with no external door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorAction {
    /// Reclaim the worktree's `target/` off the event loop, then refresh.
    CleanWorktree(std::path::PathBuf),
    /// A Containers-tab row action (stop/restart/remove/logs/shell). Dispatched
    /// by [`crate::monitor_action::dispatch`], which owns the subprocess and the
    /// pane it may open.
    Container(ContainerRequest),
    /// A Pipeline-tab row activation: jump to the dispatch's worktree.
    /// Dispatched by [`crate::monitor_action::pipeline_jump`], which owns the
    /// session/sidebar the overlay cannot reach.
    Pipeline(PipelineJump),
}

/// "Take me to this stage's work" — raised by `Enter`/click on a Pipeline row.
///
/// `session` is carried but not yet consumed: focusing the *pane* running the
/// stage (rather than its worktree) is phase 2, and the request shape is fixed
/// now so that lands without re-plumbing the escalation channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineJump {
    /// Worktree path of the dispatch row.
    pub worktree: String,
    /// The daemon session running it, when the row records one.
    pub session: Option<String>,
}

/// Rows the chrome reserves inside the box: the tab bar on top, the key-hint
/// footer on the bottom. Cached into `body_rows` so the scroll clamp and the
/// renderer can never disagree about the viewport.
const CHROME_ROWS: usize = 2;

/// Widest window that still earns the fast (500ms) sampling cadence.
///
/// Past this a plot column already covers several samples, so the extra
/// resolution is invisible while the cost is not. With a configurable ladder
/// this is a *span* threshold rather than a variant check, so a new rung is
/// classified by how wide it is rather than by where it sits in a list.
const LIVE_WINDOW_MAX_SECS: u32 = 600;

/// Which metric family the monitor is showing.
///
/// Declaration order is tab-bar order. [`MonitorTab::visible`] filters families
/// the machine doesn't expose, and the digit keys index the *visible* list — so
/// on a desktop `2` is Memory rather than an empty Power tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorTab {
    #[default]
    Cpu,
    Memory,
    Thermal,
    Network,
    Disk,
    Gpu,
    Power,
    Procs,
    /// thegn's containers across detected backends — stats + lifecycle on the
    /// owned ones. Hidden when no container engine is detected.
    Containers,
    /// The agent-pipeline board: the dispatch roster grouped by stage. Hidden
    /// until something is dispatched or a pipeline is configured.
    Pipeline,
}

impl MonitorTab {
    pub const ALL: [MonitorTab; 10] = [
        MonitorTab::Cpu,
        MonitorTab::Memory,
        MonitorTab::Thermal,
        MonitorTab::Network,
        MonitorTab::Disk,
        MonitorTab::Gpu,
        MonitorTab::Power,
        MonitorTab::Procs,
        MonitorTab::Containers,
        MonitorTab::Pipeline,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            MonitorTab::Cpu => "CPU",
            MonitorTab::Memory => "Memory",
            MonitorTab::Thermal => "Thermal",
            MonitorTab::Network => "Network",
            MonitorTab::Disk => "Disk",
            MonitorTab::Gpu => "GPU",
            MonitorTab::Power => "Power",
            MonitorTab::Procs => "Processes",
            MonitorTab::Containers => "Containers",
            MonitorTab::Pipeline => "Pipeline",
        }
    }

    /// Stable persistence slug. **Never the display label** — relabelling a tab
    /// must not orphan the preferences saved under it.
    pub fn key(self) -> &'static str {
        match self {
            MonitorTab::Cpu => "cpu",
            MonitorTab::Memory => "mem",
            MonitorTab::Thermal => "temp",
            MonitorTab::Network => "net",
            MonitorTab::Disk => "disk",
            MonitorTab::Gpu => "gpu",
            MonitorTab::Power => "power",
            MonitorTab::Procs => "procs",
            MonitorTab::Containers => "containers",
            MonitorTab::Pipeline => "pipeline",
        }
    }

    pub fn from_key(s: &str) -> Option<MonitorTab> {
        MonitorTab::ALL.into_iter().find(|t| t.key() == s)
    }

    /// The masthead widget id this tab is the expansion of — the same strings
    /// `detail::widget_detail` dispatches on, so `↵` on a stat chip lands here.
    /// `None` for Processes, which has no bar widget.
    ///
    /// The inverse of [`MonitorTab::for_widget`]; a test pins the two together
    /// so the id lists can't drift.
    #[allow(dead_code)] // drift guard, see monitor_tests
    pub fn widget_id(self) -> Option<&'static str> {
        match self {
            MonitorTab::Cpu => Some("cpu"),
            MonitorTab::Memory => Some("mem"),
            MonitorTab::Thermal => Some("temp"),
            MonitorTab::Network => Some("net"),
            MonitorTab::Disk => Some("disk"),
            MonitorTab::Gpu => Some("gpu"),
            MonitorTab::Power => Some("battery"),
            MonitorTab::Procs => None,
            MonitorTab::Containers => None,
            MonitorTab::Pipeline => None,
        }
    }

    /// The tab a masthead widget expands into. Several widgets share a tab —
    /// `swap` and `freq` are rows on Memory and CPU rather than tabs of their
    /// own.
    pub fn for_widget(w: &str) -> Option<MonitorTab> {
        match w {
            "cpu" | "load" | "freq" => Some(MonitorTab::Cpu),
            "mem" | "swap" => Some(MonitorTab::Memory),
            "temp" => Some(MonitorTab::Thermal),
            "net" => Some(MonitorTab::Network),
            "disk" => Some(MonitorTab::Disk),
            "gpu" => Some(MonitorTab::Gpu),
            "battery" => Some(MonitorTab::Power),
            // Uptime is a system fact with no tab of its own; CPU is the
            // machine-overview tab, so that is where "tell me more" lands.
            "uptime" => Some(MonitorTab::Cpu),
            _ => None,
        }
    }

    /// Whether this machine has anything to show on the tab. A tab with no data
    /// is worse than a missing one: it reads as broken. `has_containers` is
    /// `!model.containers.is_empty()` — the "a container engine is present"
    /// signal the Containers tab hides on (like GPU/Power hiding with no
    /// device). `has_pipeline` is the same idea one surface over: a roster row
    /// exists, or `[[pipeline.stages]]` is configured (see
    /// `monitor_pipeline::DispatchRoster::is_present`), so a user who has never
    /// dispatched an agent never sees an empty board.
    fn present(self, s: &StatsSnapshot, has_containers: bool, has_pipeline: bool) -> bool {
        match self {
            MonitorTab::Gpu => s.gpu_pct.is_some(),
            MonitorTab::Power => s.battery.is_some(),
            MonitorTab::Thermal => s.cpu_temp_c.is_some() || !s.temps.is_empty(),
            MonitorTab::Disk => !s.disks.is_empty(),
            MonitorTab::Network => s.net_bps.is_some() || !s.net_ifaces.is_empty(),
            MonitorTab::Containers => has_containers,
            MonitorTab::Pipeline => has_pipeline,
            // CPU, Memory and Processes are always meaningful.
            _ => true,
        }
    }

    pub fn visible(s: &StatsSnapshot, has_containers: bool, has_pipeline: bool) -> Vec<MonitorTab> {
        MonitorTab::ALL
            .into_iter()
            .filter(|t| t.present(s, has_containers, has_pipeline))
            .collect()
    }
}

/// Processes-tab sort column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcSort {
    #[default]
    Cpu,
    Rss,
    Name,
    Pid,
}

impl ProcSort {
    pub const ALL: [ProcSort; 4] = [ProcSort::Cpu, ProcSort::Rss, ProcSort::Name, ProcSort::Pid];

    pub fn label(self) -> &'static str {
        match self {
            ProcSort::Cpu => "cpu",
            ProcSort::Rss => "mem",
            ProcSort::Name => "name",
            ProcSort::Pid => "pid",
        }
    }

    pub fn key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(s: &str) -> Option<ProcSort> {
        ProcSort::ALL.into_iter().find(|p| p.key() == s)
    }

    pub fn next(self) -> ProcSort {
        let i = ProcSort::ALL.iter().position(|p| *p == self).unwrap_or(0);
        ProcSort::ALL[(i + 1) % ProcSort::ALL.len()]
    }
}

/// The three toggles a single tab remembers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabPrefs {
    pub style: GraphStyle,
    pub scale: ScaleMode,
    pub window: Window,
}

/// What a key delivered to the monitor meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorOutcome {
    Pending,
    Close,
    /// A persisted toggle changed: the loop saves [`MonitorOverlay::prefs`] and
    /// re-evaluates the sampler cadence (pause and the Processes tab both feed
    /// the ticker's gate atomics).
    PrefsChanged,
    /// A Containers-tab row action was requested. The loop pulls it with
    /// [`MonitorOverlay::take_action`] and dispatches it (lifecycle subprocess,
    /// or a pane for shell-in/logs) — the overlay can't reach the session/panes
    /// itself. Kept a unit variant so [`MonitorOutcome`] stays `Copy`.
    Action,
    /// **Not ours** — the loop should let the global keymap have this key
    /// instead of treating it as consumed.
    ///
    /// Every other outcome means "handled"; without this one the modal ate
    /// every chord it did not itself implement, which is why the chord that
    /// OPENS the monitor could never toggle it shut and `Ctrl-g` (key lock)
    /// closed the monitor instead of locking anything.
    Passthrough,
}

/// A lifecycle request raised from a Containers-tab row. Always for an OWNED
/// container — the tab offers actions only on rows where `ContainerInfo.ours`,
/// and the dispatch re-derives the [`OwnedContainer`](thegn_core::sandbox_manage::OwnedContainer)
/// witness (a foreign name yields none, so nothing runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRequest {
    pub kind: ContainerReqKind,
    pub name: String,
    /// The `ContainerInfo.backend` label (`"docker"`, `"podman"`,
    /// `"podman-rootful"`).
    pub backend: String,
    /// Whether the container was running when the action was raised (drives the
    /// double-confirm on remove).
    pub running: bool,
}

/// The row action kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerReqKind {
    Stop,
    Restart,
    /// Remove; `running` gates a second confirmation in the dispatcher.
    Remove,
    /// Tail logs into a pane.
    Logs,
    /// Shell into the container in a pane.
    Shell,
}

/// One Containers-tab row's identity, cached at rebuild so a key handler (which
/// has no model) can resolve `sel` to a container without re-reading the model.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerRowMeta {
    name: String,
    backend: String,
    ours: bool,
    running: bool,
}

/// The tabbed system-monitor modal.
pub struct MonitorOverlay {
    tab: MonitorTab,
    /// Visible tabs, recomputed on every refresh. Tab-cycling and the digit
    /// keys index **this**, never [`MonitorTab::ALL`].
    tabs: Vec<MonitorTab>,
    prefs: MonitorPrefs,
    /// The active tab's rendered body. Owned — no model borrow is held across
    /// frames, the same contract the detail popups keep.
    body: Vec<Section>,
    /// Per-tab scroll offset, so leaving a tab and returning restores the
    /// reading position.
    scroll: [usize; MonitorTab::ALL.len()],
    /// Row cursor for the list tabs (Processes, Disk and Containers).
    sel: usize,
    /// Incremental filter over the Processes list (name/pid/owner). Transient —
    /// deliberately not persisted; a filter is per-session.
    filter: String,
    /// True while `/` filter input is capturing keystrokes.
    filtering: bool,
    /// The Processes rows currently displayed, in view order — the list `sel`
    /// indexes and the signal action reads. Recomputed on rebuild so the key
    /// handler never re-derives ordering out of step with the render.
    proc_rows: Vec<procs_view::ProcRow>,
    /// The Disk-tab worktree paths currently displayed, in row order — what the
    /// clean action targets. Recomputed on rebuild.
    disk_rows: Vec<build::DiskWtRow>,
    /// A pending y/n confirmation (signal or clean); owns the footer while set.
    confirm: Option<Confirm>,
    /// The pid we last SIGTERM'd, so a second signal on the same process offers
    /// SIGKILL as a distinct escalation.
    last_termed: Option<u32>,
    /// A transient footer note (signal outcome, filter echo).
    status: Option<String>,
    /// Owned + foreign container rows behind the Containers tab, cached at
    /// rebuild so a key handler can resolve `sel` without a model borrow.
    container_rows: Vec<ContainerRowMeta>,
    /// The Pipeline board's rows in view order, cached at rebuild for exactly
    /// the same reason as `container_rows`: `sel` must index what was drawn.
    pipeline_rows: Vec<crate::monitor_pipeline::PipelineRow>,
    /// An action for the loop to perform (clean, or a Containers row action);
    /// drained via [`Self::take_action`]. ONE slot for both families — a key
    /// raises at most one action, and the loop drains after every keystroke.
    pending_action: Option<MonitorAction>,
    /// The container name armed for removal (first `x`); a second `x` on the
    /// same row confirms. Any other key disarms it.
    remove_armed: Option<String>,
    /// A transient one-keystroke footer notice — the remove confirm prompt, or
    /// an action outcome the loop pushed via [`Self::set_notice`].
    notice: Option<String>,
    /// Frozen view. The recorder keeps running underneath.
    paused: bool,
    /// Wall-clock ms the freeze began, for the footer's "paused 12s".
    paused_at: Option<i64>,
    /// Frozen "now", so a paused plot doesn't drift rightwards each frame.
    frozen_now_ms: Option<u64>,
    /// The most recent wall clock the body was built against. Pausing pins
    /// `frozen_now_ms` to this, and the footer measures the freeze from it, so
    /// neither has to read the clock from a `&mut self` key handler.
    last_now_ms: u64,
    /// Wall-clock seconds the active tab's plots actually cover, when that is
    /// less than the requested window. Drives the footer's honest
    /// "1h · 4m of history" note; `None` once history fills the window.
    covered_secs: Option<f32>,
    cols: usize,
    rows: usize,
    /// `rows - CHROME_ROWS`. What [`Self::scroll_max`] measures against.
    body_rows: usize,
}

impl MonitorOverlay {
    /// Open at `tab`, sizing against `ctx.screen`.
    pub fn open(
        tab: MonitorTab,
        prefs: MonitorPrefs,
        model: &FrameModel,
        ctx: &StatusCtx,
    ) -> MonitorOverlay {
        let (cols, rows) = Self::dims(ctx.screen);
        let tabs = MonitorTab::visible(
            &model.stats,
            !model.containers.is_empty(),
            model.dispatches.is_present(),
        );
        // Opening at a tab this machine can't show would present an empty box;
        // fall back to the first real one.
        let tab = if tabs.contains(&tab) {
            tab
        } else {
            tabs.first().copied().unwrap_or(MonitorTab::Cpu)
        };
        let mut ov = MonitorOverlay {
            tab,
            tabs,
            prefs,
            body: Vec::new(),
            scroll: [0; MonitorTab::ALL.len()],
            sel: 0,
            filter: String::new(),
            filtering: false,
            proc_rows: Vec::new(),
            disk_rows: Vec::new(),
            container_rows: Vec::new(),
            pipeline_rows: Vec::new(),
            confirm: None,
            last_termed: None,
            status: None,
            pending_action: None,
            remove_armed: None,
            notice: None,
            paused: false,
            paused_at: None,
            frozen_now_ms: None,
            last_now_ms: ctx.now_ms.max(0) as u64,
            covered_secs: None,
            cols,
            rows,
            body_rows: rows.saturating_sub(CHROME_ROWS),
        };
        ov.rebuild(model, ctx);
        ov
    }

    /// Box interior size, clamped exactly the way `layer::box_dims` will clamp
    /// it. The two must agree: `rows` is what the scroll clamp measures content
    /// against, so leaving it at a size the layer then shrinks would strand the
    /// tail of a long tab out of reach.
    fn dims(screen: Rect) -> (usize, usize) {
        let cols = (screen.cols * 4 / 5)
            .max(56)
            .min(screen.cols.saturating_sub(6))
            .max(1);
        let rows = (screen.rows * 4 / 5)
            .max(16)
            .min(screen.rows.saturating_sub(3))
            .max(1);
        (cols, rows)
    }

    /// The active tab, for the loop's sampler gating.
    #[allow(dead_code)] // read by tests and future gating
    pub fn tab(&self) -> MonitorTab {
        self.tab
    }

    #[allow(dead_code)] // read by tests
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// True while the monitor wants live (fast-cadence) sampling: it is open,
    /// unpaused, and showing a window short enough for the extra resolution to
    /// be visible. An hour-wide plot gains nothing from 500ms samples.
    pub fn wants_live(&self) -> bool {
        !self.paused
            && self
                .prefs
                .tab(self.tab)
                .window
                .secs()
                .is_some_and(|s| s <= LIVE_WINDOW_MAX_SECS)
    }

    /// True while the Processes tab is the live view — the gate for the
    /// expensive full process enumeration.
    pub fn wants_procs(&self) -> bool {
        self.tab == MonitorTab::Procs && !self.paused
    }

    /// True while the Containers tab is the live view — the gate for the
    /// expensive per-container `stats --no-stream` sampling (and the aggregate
    /// `df`). Closed monitor ⇒ false ⇒ no stats subprocess (the always-on cost
    /// this change removes).
    pub fn wants_container_stats(&self) -> bool {
        self.tab == MonitorTab::Containers && !self.paused
    }

    /// True while the Pipeline board is the live view — the gate for the
    /// off-loop roster sample. Closed monitor (or any other tab) ⇒ false ⇒ no
    /// periodic DB read at all, which is what keeps the board free when nobody
    /// is looking at it.
    pub fn wants_dispatches(&self) -> bool {
        self.tab == MonitorTab::Pipeline && !self.paused
    }

    pub fn prefs(&self) -> &MonitorPrefs {
        &self.prefs
    }

    /// Drain a pending loop-side action (a Disk-tab clean, or a Containers-tab
    /// row action), if any. Called by the loop after every key so the overlay
    /// never touches the DB, spawns a subprocess, or opens a pane itself.
    pub fn take_action(&mut self) -> Option<MonitorAction> {
        self.pending_action.take()
    }

    /// The Processes list's current view toggles, for [`procs_view::rows`].
    fn proc_view(&self) -> ProcSnapshotView {
        ProcSnapshotView {
            sort: self.prefs.proc_sort,
            desc: self.prefs.proc_desc,
            filter: self.filter.clone(),
            tree: self.prefs.proc_tree,
        }
    }

    /// Rebuild the active tab's body from current data.
    fn rebuild(&mut self, model: &FrameModel, ctx: &StatusCtx) {
        let live_now = ctx.now_ms.max(0) as u64;
        self.last_now_ms = live_now;
        let now = self.frozen_now_ms.unwrap_or(live_now);
        // Recompute EVERY list tab's rows first — Processes, Disk and Containers
        // — so `sel`, the signal action, the clean action and the container row
        // actions all index exactly what the renderer draws. Rows first, then
        // one clamp for the active tab, then the build.
        self.proc_rows = procs_view::rows(&model.procs, self.proc_view());
        self.disk_rows = build::worktree_disk_rows(model, now / 1000);
        // Container row identities for the key handler (the same order the
        // builder renders `model.containers` in), so a key resolves `sel`
        // without a model borrow.
        if self.tab == MonitorTab::Containers {
            self.container_rows = model
                .containers
                .iter()
                .map(|c| ContainerRowMeta {
                    name: c.name.clone(),
                    backend: c.backend.clone(),
                    ours: c.ours,
                    running: thegn_core::sandbox_manage::container_running(&c.status),
                })
                .collect();
        }
        // Same contract for the board: the row list the key handler resolves
        // `sel` against is the exact list the builder is about to draw.
        if self.tab == MonitorTab::Pipeline {
            self.pipeline_rows = crate::monitor_pipeline::ordered_rows(
                &model.dispatches.rows,
                &model.dispatches.stage_order,
                now as i64,
            );
        }
        self.clamp_sel();
        // Bind the body to a local so the immutable borrows of `self.proc_rows`
        // / `self.filter` in the argument end before `self.body` is assigned.
        let body = build::tab(build::TabInput {
            tab: self.tab,
            model,
            hist: ctx.hist,
            prefs: self.prefs.tab(self.tab),
            cols: self.cols,
            now_ms: now,
            sel: self.sel,
            filter: &self.filter,
            filtering: self.filtering,
            tree: self.prefs.proc_tree,
            proc_sort: self.prefs.proc_sort,
            proc_desc: self.prefs.proc_desc,
            proc_rows: &self.proc_rows,
            disk_rows: &self.disk_rows,
            pipeline_rows: &self.pipeline_rows,
            disk_eta: ctx.hist.disk_fill_eta(),
        });
        self.body = body;
        self.covered_secs = ctx.hist.coverage_secs(now, self.prefs.tab(self.tab).window);
        self.clamp();
    }

    /// Keep the row cursor inside the current list tab's rows. The list shrinks
    /// under the user (a process exits, a worktree is cleaned, a container is
    /// removed), and a stranded cursor would act on the wrong row or none.
    fn clamp_sel(&mut self) {
        let len = self.row_len();
        self.sel = self.sel.min(len.saturating_sub(1));
    }

    /// Keep the viewport and row cursor inside the current body. The stack can
    /// shrink under the user (a disk unmounts, a process exits), and stranding
    /// the viewport past the end would look like a frozen modal.
    fn clamp(&mut self) {
        let max = self.scroll_max();
        let s = &mut self.scroll[self.tab.index()];
        *s = (*s).min(max);
    }

    fn scroll_max(&self) -> usize {
        sections::stack_height(&self.body).saturating_sub(self.body_rows)
    }

    fn scroll(&self) -> usize {
        self.scroll[self.tab.index()]
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.scroll_max() as isize;
        let s = &mut self.scroll[self.tab.index()];
        *s = (*s as isize + delta).clamp(0, max) as usize;
    }

    /// Wheel scrolling, for the mouse path.
    pub fn wheel(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    /// Jump an already-open monitor to `tab` and repaint it.
    ///
    /// Returns `false` (and moves nothing) when this machine doesn't show that
    /// tab — landing the user on an unrelated family would be worse than the
    /// action appearing to do nothing, and the caller reports why.
    pub fn goto_tab(&mut self, tab: MonitorTab, model: &FrameModel, ctx: &StatusCtx) -> bool {
        if !self.tabs.contains(&tab) {
            return false;
        }
        self.tab = tab;
        self.sel = 0;
        self.remember_tab();
        self.rebuild_after_key(model, ctx);
        true
    }

    /// Rebuild after a key that changed what should be on screen — a tab
    /// switch, a style/scale/window toggle, a re-sort.
    ///
    /// Distinct from [`Self::refresh`] because it must run **even while
    /// paused**: freezing the data must not freeze the navigation. It rebuilds
    /// against the frozen clock, so a paused plot still shows the instant it
    /// was paused at.
    pub fn rebuild_after_key(&mut self, model: &FrameModel, ctx: &StatusCtx) {
        self.resize(ctx.screen);
        self.rebuild(model, ctx);
    }

    /// Rebuild in place from fresh data. Returns `true` when it repainted.
    ///
    /// Called from the loop's stats drain — the same drain
    /// `detail::status_modal::refresh_open` already rides — so an open monitor
    /// costs no new wake source. A paused monitor returns immediately and
    /// touches nothing, which is also what keeps a frozen picture from
    /// re-dirtying the frame at the sample rate.
    pub fn refresh(&mut self, model: &FrameModel, ctx: &StatusCtx) -> bool {
        if self.paused {
            return false;
        }
        self.resize(ctx.screen);
        self.tabs = MonitorTab::visible(
            &model.stats,
            !model.containers.is_empty(),
            model.dispatches.is_present(),
        );
        if !self.tabs.contains(&self.tab) {
            // The metric vanished under the user (GPU driver unloaded, battery
            // removed). Fall back rather than render an empty tab.
            self.tab = self.tabs.first().copied().unwrap_or(MonitorTab::Cpu);
        }
        self.rebuild(model, ctx);
        true
    }

    /// Re-clamp to a resized terminal.
    fn resize(&mut self, screen: Rect) {
        let (cols, rows) = Self::dims(screen);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.body_rows = rows.saturating_sub(CHROME_ROWS);
        }
    }

    fn switch(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let cur = self.tabs.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.tab = self.tabs[(((cur + delta) % n + n) % n) as usize];
        self.sel = 0;
        self.remember_tab();
    }

    /// Record the tab the monitor should reopen on.
    ///
    /// `MonitorPrefs::last_tab` is persisted and read back by the loop when it
    /// reopens the overlay, but nothing ever wrote it — so "reopen where you
    /// left off" always reopened on CPU. Called from every path that moves the
    /// tab (the arrows/Tab, the digits, and the direct `goto_tab` door).
    fn remember_tab(&mut self) {
        self.prefs.last_tab = self.tab;
    }
}

/// Whether the ticker should sample at its fast half-tick: only while a human
/// is looking at a live surface.
///
/// Pausing the monitor therefore *reduces* the wake rate back to the configured
/// cadence, and closing it drops the fast path entirely — the 0%-idle contract
/// holds by construction rather than by remembering to clear a flag.
pub fn wants_live_stats(telemetry_section_open: bool, monitor: Option<&MonitorOverlay>) -> bool {
    telemetry_section_open || monitor.is_some_and(|m| m.wants_live())
}

/// Whether the ticker should run the expensive full process enumeration.
///
/// `cfg_enabled` is `[monitor] processes`, the kill switch for a machine where
/// the sweep is too costly.
pub fn wants_process_scan(monitor: Option<&MonitorOverlay>, cfg_enabled: bool) -> bool {
    cfg_enabled && monitor.is_some_and(|m| m.wants_procs())
}

/// Reclaim a worktree's `target/` off the event loop (the manual sibling of
/// `[disk] auto_clean_on_merge`), drop its now-stale size-cache row, and pulse
/// the waker so the sidebar/monitor repaint. Background QoS — housekeeping, not
/// interactive. A clean that fails *while running* is logged, never a crash.
///
/// Returns `Err` when the worker thread could not even be spawned. The user has
/// already CONFIRMED a destructive action by this point, so "nothing happened"
/// must not read as "it worked": the caller surfaces the failure, and it is
/// logged here beside `clean_target`'s own failures.
pub fn spawn_clean(
    path: std::path::PathBuf,
    waker: termwiz::terminal::TerminalWaker,
) -> std::io::Result<()> {
    let shown = path.display().to_string();
    std::thread::Builder::new()
        .name("thegn-monitor-clean".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            match thegn_core::worktree::clean_target(&path) {
                Ok(reclaimed) => {
                    // Drop the stale badge immediately; the next disk scan
                    // remeasures. best-effort: the DB is a cache.
                    if let Ok(db) = thegn_core::db::Db::open() {
                        use thegn_core::store::WorktreeAuxStore;
                        let _ = db.delete_worktree_disk(&path.to_string_lossy());
                    }
                    tracing::info!(
                        target: "thegn::disk", path = %path.display(), reclaimed,
                        "monitor cleaned worktree target/"
                    );
                }
                Err(e) => tracing::warn!(
                    target: "thegn::disk", path = %path.display(), "monitor clean failed: {e}"
                ),
            }
            let _ = waker.wake();
        })
        .map(|_| ())
        .inspect_err(|e| {
            tracing::warn!(
                target: "thegn::disk", path = %shown,
                "monitor clean thread could not be spawned: {e}"
            );
        })
}

/// Whether a per-container-stats surface is visible, so the ambient container
/// tick should enrich its `ps` with `stats`/`df`. True while the monitor's
/// Containers tab is live, OR the Sandbox panel section is open (its expanded
/// stats show per-container numbers too). Closed ⇒ the tick keeps only the
/// cheap listing — the visibility gate that removes the standing `stats` cost.
pub fn wants_container_stats(monitor: Option<&MonitorOverlay>, sandbox_section_open: bool) -> bool {
    sandbox_section_open || monitor.is_some_and(|m| m.wants_container_stats())
}

// --- Key handling --------------------------------------------------------

impl MonitorOverlay {
    /// Dispatch one key.
    ///
    /// Order is modifiers → close → tab switch → global toggles → navigation →
    /// per-tab. The per-tab arm runs **last** on purpose: a future row action
    /// can then never shadow a global key, and the Processes tab's sort letters
    /// cannot swallow `g`/`s`/`q`.
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> MonitorOutcome {
        // ORDER (deliberate, three steps):
        //
        // (a) The one-keystroke footer notice clears FIRST. Any key dismisses
        //     it, in every mode — a notice is pure display, so clearing it can
        //     never change what the key then means.
        // (b) The two sub-modes own every key while active, so a `/` filter or
        //     a y/n confirmation is never half-swallowed by the global handlers
        //     below. They return before (c).
        // (c) The remove-arm disarm stays in the NORMAL flow, after the
        //     early-returns: a sub-mode keystroke therefore cannot disarm a
        //     pending remove. That is safe because filtering/confirm live on the
        //     Processes and Disk tabs while remove-arming lives on Containers —
        //     they cannot be active at the same time in practice. (And a stale
        //     arm is harmless: `container_key` re-checks the armed NAME against
        //     the selected row, so it can only ever confirm the same container.)
        self.notice = None;
        if self.filtering {
            return self.filter_key(key);
        }
        if self.confirm.is_some() {
            return self.confirm_key(key);
        }
        // (c) A pending remove stays armed only across a repeated `x`; any other
        // key disarms it, so a stray keypress can never become a confirmed
        // removal.
        if !matches!(key, KeyCode::Char('x')) {
            self.remove_armed = None;
        }
        // Alt/Super chords belong to the compositor, not to us — and that means
        // HANDING THEM BACK, not swallowing them. Checked before CTRL so a
        // `Ctrl Alt …` chord (the monitor's own open chord among them) passes
        // too, which is what lets the opening chord toggle the modal shut.
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return MonitorOutcome::Passthrough;
        }
        if mods.contains(Modifiers::CTRL) {
            return match key {
                // Ctrl-C is the universal "get me out of here".
                KeyCode::Char('c' | 'C') => MonitorOutcome::Close,
                // Ctrl-G is the global key-lock toggle; closing the monitor was
                // never what it meant. Hand it back.
                KeyCode::Char('g' | 'G') => MonitorOutcome::Passthrough,
                _ => MonitorOutcome::Pending,
            };
        }
        if crate::input::is_escape_key(key) {
            return MonitorOutcome::Close;
        }
        let shift = mods.contains(Modifiers::SHIFT);
        let page = self.body_rows.saturating_sub(1).max(1) as isize;

        match key {
            KeyCode::Char('q') => MonitorOutcome::Close,

            // --- Tabs ---
            // `PrefsChanged`, not `Pending`: the tab IS a persisted preference
            // (`MonitorPrefs::last_tab`, what the next open lands on), and that
            // outcome is the loop's only door to saving prefs — the same one the
            // window/style/scale toggles use.
            KeyCode::Tab if shift => {
                self.switch(-1);
                MonitorOutcome::PrefsChanged
            }
            KeyCode::Tab | KeyCode::Char('\t') => {
                self.switch(1);
                MonitorOutcome::PrefsChanged
            }
            KeyCode::RightArrow | KeyCode::Char('l') => {
                self.switch(1);
                MonitorOutcome::PrefsChanged
            }
            KeyCode::LeftArrow | KeyCode::Char('h') => {
                self.switch(-1);
                MonitorOutcome::PrefsChanged
            }
            // Digits index the VISIBLE tabs, so `2` means the same thing on a
            // laptop and a GPU-less server. Out of range is a no-op.
            KeyCode::Char(c @ '1'..='9') => {
                let i = (*c as usize) - ('1' as usize);
                if let Some(t) = self.tabs.get(i).copied() {
                    self.tab = t;
                    self.sel = 0;
                    self.remember_tab();
                    return MonitorOutcome::PrefsChanged;
                }
                MonitorOutcome::Pending
            }

            // --- Global toggles ---
            // `Space` is pause, NOT page-down (PgDn pages). A monitor you can't
            // freeze is one you can't read a spike off.
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                if self.paused {
                    self.paused_at = Some(self.last_now_ms as i64);
                    // Pin "now" so the frozen plot doesn't creep rightwards.
                    self.frozen_now_ms = Some(self.last_now_ms);
                } else {
                    self.paused_at = None;
                    self.frozen_now_ms = None;
                }
                MonitorOutcome::PrefsChanged
            }
            // `g` is graph style, so go-to-top is Home/`G` only.
            KeyCode::Char('g') => {
                let p = self.prefs.tab_mut(self.tab);
                p.style = p.style.next();
                MonitorOutcome::PrefsChanged
            }
            // `s` is scale — global across every tab, which is why the
            // Processes sort keys are c/m/n/r and never `s`.
            KeyCode::Char('s') => {
                let p = self.prefs.tab_mut(self.tab);
                p.scale = p.scale.next();
                MonitorOutcome::PrefsChanged
            }
            KeyCode::Char('[') => {
                self.prefs.narrow(self.tab);
                MonitorOutcome::PrefsChanged
            }
            KeyCode::Char(']') => {
                self.prefs.widen(self.tab);
                MonitorOutcome::PrefsChanged
            }

            // --- Navigation ---
            KeyCode::DownArrow | KeyCode::Char('j') => {
                self.nav(1);
                MonitorOutcome::Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.nav(-1);
                MonitorOutcome::Pending
            }
            KeyCode::PageDown => {
                self.nav(page);
                MonitorOutcome::Pending
            }
            KeyCode::PageUp => {
                self.nav(-page);
                MonitorOutcome::Pending
            }
            KeyCode::Home => {
                self.scroll[self.tab.index()] = 0;
                self.sel = 0;
                MonitorOutcome::Pending
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll[self.tab.index()] = self.scroll_max();
                MonitorOutcome::Pending
            }

            // --- Row actions (before the per-tab sort letters) ---
            // `/` opens the incremental process filter.
            KeyCode::Char('/') if self.tab == MonitorTab::Procs => {
                self.filtering = true;
                self.status = None;
                MonitorOutcome::Pending
            }
            // `t` toggles process-tree grouping (persisted).
            KeyCode::Char('t') if self.tab == MonitorTab::Procs => {
                self.prefs.proc_tree = !self.prefs.proc_tree;
                self.sel = 0;
                MonitorOutcome::PrefsChanged
            }
            // `x` acts on the selected row: signal a process, or clean a
            // worktree. Both open a confirmation rather than firing immediately.
            KeyCode::Char('x') if self.tab == MonitorTab::Procs => self.begin_signal(),
            KeyCode::Char('x') if self.tab == MonitorTab::Disk => self.begin_clean(),

            // --- Per-tab (last, so it can never shadow the above) ---
            KeyCode::Char(c) if self.tab == MonitorTab::Procs => self.proc_key(*c),
            KeyCode::Enter if self.tab == MonitorTab::Containers => self.container_key('\r'),
            KeyCode::Char(c) if self.tab == MonitorTab::Containers => self.container_key(*c),
            KeyCode::Enter if self.tab == MonitorTab::Pipeline => self.pipeline_key(),
            _ => MonitorOutcome::Pending,
        }
    }

    /// Scroll, or move the row cursor on a list tab (Processes, Disk,
    /// Containers).
    fn nav(&mut self, delta: isize) {
        // All three list tabs move the row cursor, and all three clamp at BOTH
        // ends against their own row count — a cursor past the last row would
        // act on nothing (or, worse, on a row that scrolled away).
        if matches!(
            self.tab,
            MonitorTab::Procs | MonitorTab::Disk | MonitorTab::Containers | MonitorTab::Pipeline
        ) {
            let len = self.row_len();
            let max = len.saturating_sub(1) as isize;
            self.sel = (self.sel as isize + delta).clamp(0, max.max(0)) as usize;
        }
        self.scroll_by(delta);
    }

    /// How many rows the active LIST tab is showing — the single source the
    /// cursor clamp and the navigation bound both measure against, so they can
    /// never disagree about where the list ends.
    fn row_len(&self) -> usize {
        match self.tab {
            MonitorTab::Procs => self.proc_rows.len(),
            MonitorTab::Disk => self.disk_rows.len(),
            MonitorTab::Containers => self.container_rows.len(),
            MonitorTab::Pipeline => self.pipeline_rows.len(),
            _ => 0,
        }
    }

    /// Processes-tab sort keys. Reached only after every global key has had its
    /// chance.
    fn proc_key(&mut self, c: char) -> MonitorOutcome {
        let sort = match c {
            'c' => ProcSort::Cpu,
            'm' => ProcSort::Rss,
            'n' => ProcSort::Name,
            'p' => ProcSort::Pid,
            '>' | '<' => self.prefs.proc_sort.next(),
            'r' => {
                self.prefs.proc_desc = !self.prefs.proc_desc;
                return MonitorOutcome::PrefsChanged;
            }
            _ => return MonitorOutcome::Pending,
        };
        self.prefs.proc_sort = sort;
        self.sel = 0;
        MonitorOutcome::PrefsChanged
    }

    /// Filter-input sub-mode: edit `self.filter`, or leave it. Every key is
    /// consumed here so a typed `q`/`g`/`s` lands in the query, not the global
    /// handlers.
    fn filter_key(&mut self, key: &KeyCode) -> MonitorOutcome {
        if crate::input::is_escape_key(key) {
            // Esc cancels the filter (and clears it), rather than closing the
            // monitor — you back out of the filter first.
            self.filtering = false;
            self.filter.clear();
            self.sel = 0;
            return MonitorOutcome::Pending;
        }
        match key {
            KeyCode::Enter | KeyCode::Char('\r' | '\n') => {
                // Accept: keep the filter applied, leave input mode.
                self.filtering = false;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.sel = 0;
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.filter.push(*c);
                self.sel = 0;
            }
            _ => {}
        }
        MonitorOutcome::Pending
    }

    /// Confirmation sub-mode: `y` performs, `n`/Esc cancels, anything else
    /// leaves the prompt standing.
    fn confirm_key(&mut self, key: &KeyCode) -> MonitorOutcome {
        if matches!(key, KeyCode::Char('y' | 'Y')) {
            match self.confirm.take() {
                Some(Confirm::Signal { pid, label, stage }) => {
                    self.perform_signal(pid, &label, stage)
                }
                Some(Confirm::Clean { path, label }) => {
                    self.pending_action = Some(MonitorAction::CleanWorktree(path));
                    self.status = Some(format!("cleaning {label}…"));
                }
                None => {}
            }
            return MonitorOutcome::Pending;
        }
        if crate::input::is_escape_key(key) || matches!(key, KeyCode::Char('n' | 'N')) {
            self.confirm = None;
            self.status = Some("cancelled".into());
        }
        MonitorOutcome::Pending
    }

    /// Open a signal confirmation for the selected process. A second signal on
    /// the just-TERMed pid escalates to KILL (a distinct, explicit second step).
    fn begin_signal(&mut self) -> MonitorOutcome {
        // Copy out of the row first so the immutable borrow of `self.proc_rows`
        // ends before `self.confirm`/`self.status` are written.
        let Some((pid, name, owner)) = self
            .proc_rows
            .get(self.sel)
            .map(|r| (r.pid, r.name.clone(), r.owner))
        else {
            return MonitorOutcome::Pending;
        };
        let stage = if self.last_termed == Some(pid) {
            crate::platform::ProcSignal::Kill
        } else {
            crate::platform::ProcSignal::Terminate
        };
        let owner = procs_view::owner_label(owner);
        let owner = if owner.is_empty() {
            String::new()
        } else {
            format!(" ({owner})")
        };
        let label = format!("pid {pid} {name}{owner}");
        self.confirm = Some(Confirm::Signal { pid, label, stage });
        self.status = None;
        MonitorOutcome::Pending
    }

    /// Deliver the confirmed signal, surfacing the outcome — never swallowed.
    fn perform_signal(&mut self, pid: u32, label: &str, stage: crate::platform::ProcSignal) {
        let name = match stage {
            crate::platform::ProcSignal::Terminate => "SIGTERM",
            crate::platform::ProcSignal::Kill => "SIGKILL",
        };
        match crate::platform::signal_pid(pid, stage) {
            Ok(()) => {
                self.status = Some(format!("sent {name} to {label}"));
                if stage == crate::platform::ProcSignal::Terminate {
                    self.last_termed = Some(pid);
                }
            }
            Err(e) => self.status = Some(format!("{label}: {e}")),
        }
    }

    /// Open a clean confirmation for the selected worktree row.
    fn begin_clean(&mut self) -> MonitorOutcome {
        let Some(path) = self.disk_rows.get(self.sel).map(|r| r.path.clone()) else {
            return MonitorOutcome::Pending;
        };
        let label = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.confirm = Some(Confirm::Clean { path, label });
        self.status = None;
        MonitorOutcome::Pending
    }

    /// Containers-tab row actions. Reached only after every global key; offered
    /// only on an OWNED row (foreign containers are read-only, so their keys are
    /// no-ops). Records the request and hands the loop [`MonitorOutcome::Action`]
    /// — the overlay can't spawn a subprocess or open a pane itself.
    ///
    /// Keys: `t` stop, `r` restart, `x` remove, `o` logs, `Enter` shell-in.
    /// (`s`/`g`/`l` are global — scale/graph/switch — so stop is `t`, not `s`.)
    fn container_key(&mut self, c: char) -> MonitorOutcome {
        let kind = match c {
            't' => ContainerReqKind::Stop,
            'r' => ContainerReqKind::Restart,
            'x' => ContainerReqKind::Remove,
            'o' => ContainerReqKind::Logs,
            '\r' | '\n' => ContainerReqKind::Shell,
            _ => return MonitorOutcome::Pending,
        };
        let Some(row) = self.container_rows.get(self.sel).cloned() else {
            return MonitorOutcome::Pending;
        };
        // Actions are offered only on OWNED rows — foreign containers are
        // read-only on every surface.
        if !row.ours {
            return MonitorOutcome::Pending;
        }
        // Remove is a two-press confirm; a running container gets a force
        // warning (the "second confirmation when running" the spec calls for).
        if kind == ContainerReqKind::Remove
            && self.remove_armed.as_deref() != Some(row.name.as_str())
        {
            self.remove_armed = Some(row.name.clone());
            self.notice = Some(if row.running {
                format!("remove RUNNING {}? press x again to force-remove", row.name)
            } else {
                format!("remove {}? press x again to confirm", row.name)
            });
            return MonitorOutcome::Pending;
        }
        self.remove_armed = None;
        self.pending_action = Some(MonitorAction::Container(ContainerRequest {
            kind,
            name: row.name,
            backend: row.backend,
            running: row.running,
        }));
        MonitorOutcome::Action
    }

    /// Pipeline-tab row activation (`Enter`, and the mouse click that routes
    /// here). Records a jump request and hands the loop
    /// [`MonitorOutcome::Action`] — the overlay can reach neither the session
    /// nor the sidebar. Read-only: the board never mutates the roster, because
    /// thegn never advances a stage (that is the supervising agent's judgment).
    pub fn pipeline_key(&mut self) -> MonitorOutcome {
        let Some(row) = self.pipeline_rows.get(self.sel) else {
            return MonitorOutcome::Pending;
        };
        if row.worktree_path.is_empty() {
            return MonitorOutcome::Pending;
        }
        self.pending_action = Some(MonitorAction::Pipeline(PipelineJump {
            worktree: row.worktree_path.clone(),
            session: row.session_id.clone(),
        }));
        MonitorOutcome::Action
    }

    /// The loop pushes an action outcome (or immediate confirmation) here; it
    /// shows in the footer until the next keystroke.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;

// --- Rendering -----------------------------------------------------------

impl MonitorOverlay {
    fn spec(&self) -> LayerSpec {
        LayerSpec {
            title: "system monitor".into(),
            badge: Some(" esc ".into()),
            cols: self.cols,
            rows: self.rows,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Faint),
        }
    }

    /// The outer box, for mouse hit-testing. Shares `spec` with `render`, so
    /// the click target can never drift from what was drawn.
    pub fn box_rect(&self, screen: Rect) -> Option<Rect> {
        layer::box_rect(&self.spec(), screen)
    }

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let Some(inner) = layer::open_layer(surface, screen, &self.spec()) else {
            return;
        };
        // Row 0: tabs. Last row: hints. Everything between: the scrolled stack.
        crate::seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &self.tab_bar(),
            sections::panel(),
        );
        let body = Rect {
            x: inner.x,
            y: inner.y + 1,
            cols: inner.cols,
            rows: inner.rows.saturating_sub(CHROME_ROWS),
        };
        sections::render_stack(surface, body, self.scroll(), &self.body);
        crate::seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows.saturating_sub(1),
            inner.cols,
            &self.footer(),
            sections::panel(),
        );
    }

    /// `CPU  Memory  Network …` with the active tab accented and underlined,
    /// plus a right-aligned pause marker.
    fn tab_bar(&self) -> Line {
        let mut left: Vec<Seg> = Vec::new();
        for (i, t) in self.tabs.iter().enumerate() {
            if i > 0 {
                left.push(seg(Tok::Slot(S::Ghost), "  "));
            }
            if *t == self.tab {
                left.push(seg(Tok::Slot(S::Accent), t.label()).bold());
            } else {
                left.push(seg(Tok::Slot(S::Dim), t.label()));
            }
        }
        let right = if self.paused {
            vec![seg(
                Tok::Slot(S::Accent),
                format!("⏸ paused {}", self.paused_for()),
            )]
        } else {
            vec![seg(Tok::Slot(S::Ghost), self.coverage_note())]
        };
        Line::split(left, right)
    }

    /// How long the freeze has lasted, e.g. `12s` / `2m`.
    fn paused_for(&self) -> String {
        let secs = self
            .paused_at
            .map(|at| (self.last_now_ms as i64 - at).max(0) / 1000)
            .unwrap_or(0);
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m", secs / 60)
        }
    }

    /// `2m` — or `1h · 4m of history` when the ring holds less than the window
    /// asks for, so a wide window never implies data it doesn't have.
    fn coverage_note(&self) -> String {
        let p = self.prefs.tab(self.tab);
        let want = p.window;
        match (want.secs(), self.covered_secs) {
            // `w as f32`, NOT `f32::from(w as u16)`: the u16 cast silently
            // truncates any window past 18h12m, so a wide rung would compare
            // against a wrapped span and claim full coverage it doesn't have.
            (Some(w), Some(c)) if c + 5.0 < w as f32 => {
                format!("{} · {} of history", want.label(), fmt_secs(c))
            }
            _ => want.label(),
        }
    }

    /// The key-hint footer — or, when one is active, a confirmation prompt, the
    /// filter input, or a transient status note.
    fn footer(&self) -> Line {
        // A pending confirmation owns the footer: it names exactly what will
        // happen, so a pane-owned build is recognizably thegn's own.
        if let Some(c) = &self.confirm {
            let msg = match c {
                Confirm::Signal { label, stage, .. } => {
                    let verb = match stage {
                        crate::platform::ProcSignal::Terminate => "terminate",
                        crate::platform::ProcSignal::Kill => "KILL (no cleanup)",
                    };
                    format!("{verb} {label}?")
                }
                Confirm::Clean { label, .. } => format!("clean target/ in {label}?"),
            };
            return Line::split(
                vec![seg(Tok::Slot(S::Accent), msg).bold()],
                vec![
                    Seg::key("y"),
                    seg(Tok::Slot(S::Ghost), " yes  "),
                    Seg::key("n"),
                    seg(Tok::Slot(S::Ghost), " no"),
                ],
            );
        }
        // Filter input: echo the query with a cursor.
        if self.filtering {
            return Line::split(
                vec![
                    Seg::key("/"),
                    seg(Tok::Slot(S::Ghost), " filter "),
                    seg(Tok::Slot(S::Accent), format!("{}\u{2502}", self.filter)),
                ],
                vec![seg(
                    Tok::Slot(S::Ghost),
                    "esc clear · enter apply".to_string(),
                )],
            );
        }
        // A pending container confirm / action outcome takes over the footer
        // while set.
        if let Some(notice) = &self.notice {
            return Line::split(
                vec![seg(Tok::Slot(S::Accent), notice.clone())],
                vec![seg(Tok::Slot(S::Ghost), "q close".to_string())],
            );
        }
        // The board is a read-only table: one action, and the graph toggles
        // would mean nothing here either.
        if self.tab == MonitorTab::Pipeline {
            let left = vec![
                Seg::key("tab"),
                seg(Tok::Slot(S::Ghost), " tabs  "),
                Seg::key("↵"),
                seg(Tok::Slot(S::Ghost), " go to worktree"),
            ];
            return Line::split(left, vec![seg(Tok::Slot(S::Ghost), "q close".to_string())]);
        }
        // The Containers tab has its own action legend rather than the graph
        // toggles (which mean nothing for a table).
        if self.tab == MonitorTab::Containers {
            let owned = self.container_rows.get(self.sel).is_some_and(|r| r.ours);
            let mut left = vec![Seg::key("tab"), seg(Tok::Slot(S::Ghost), " tabs  ")];
            if owned {
                left.extend([
                    Seg::key("↵"),
                    seg(Tok::Slot(S::Ghost), " shell  "),
                    Seg::key("o"),
                    seg(Tok::Slot(S::Ghost), " logs  "),
                    Seg::key("t"),
                    seg(Tok::Slot(S::Ghost), " stop  "),
                    Seg::key("r"),
                    seg(Tok::Slot(S::Ghost), " restart  "),
                    Seg::key("x"),
                    seg(Tok::Slot(S::Ghost), " remove"),
                ]);
            } else {
                left.push(seg(Tok::Slot(S::Ghost), "foreign container — read-only"));
            }
            return Line::split(left, vec![seg(Tok::Slot(S::Ghost), "q close".to_string())]);
        }
        let p = self.prefs.tab(self.tab);
        let mut left = vec![
            Seg::key("tab"),
            seg(Tok::Slot(S::Ghost), " tabs  "),
            Seg::key("[ ]"),
            seg(Tok::Slot(S::Ghost), format!(" {}  ", p.window.label())),
            Seg::key("g"),
            seg(Tok::Slot(S::Ghost), format!(" {}  ", p.style.label())),
            Seg::key("s"),
            seg(Tok::Slot(S::Ghost), format!(" {}  ", p.scale.label())),
            Seg::key("spc"),
            seg(
                Tok::Slot(S::Ghost),
                if self.paused { " resume" } else { " pause" },
            ),
        ];
        if self.tab == MonitorTab::Procs {
            left.push(seg(Tok::Slot(S::Ghost), "  "));
            left.push(Seg::key("c/m/n"));
            left.push(seg(
                Tok::Slot(S::Ghost),
                format!(
                    " sort {}{}  ",
                    self.prefs.proc_sort.label(),
                    if self.prefs.proc_desc { "↓" } else { "↑" }
                ),
            ));
            left.push(Seg::key("/"));
            left.push(seg(Tok::Slot(S::Ghost), " find  "));
            left.push(Seg::key("t"));
            left.push(seg(
                Tok::Slot(S::Ghost),
                if self.prefs.proc_tree {
                    " flat  "
                } else {
                    " tree  "
                },
            ));
            left.push(Seg::key("x"));
            left.push(seg(Tok::Slot(S::Ghost), " signal"));
        } else if self.tab == MonitorTab::Disk && !self.disk_rows.is_empty() {
            left.push(seg(Tok::Slot(S::Ghost), "  "));
            left.push(Seg::key("x"));
            left.push(seg(Tok::Slot(S::Ghost), " clean"));
        }
        // A transient status note (signal outcome, filter echo) takes the right
        // slot over the close hint — it is the thing the user just asked for.
        let right = match &self.status {
            Some(s) => seg(Tok::Slot(S::Accent), s.clone()),
            None => seg(Tok::Slot(S::Ghost), "q close".to_string()),
        };
        Line::split(left, vec![right])
    }
}

fn fmt_secs(s: f32) -> String {
    let s = s.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}
