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
/// resources the overlay doesn't hold (a background thread + the DB). The signal
/// action is NOT here — it is a self-contained syscall the overlay makes
/// directly, which is what keeps it TUI-only with no external door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorAction {
    /// Reclaim the worktree's `target/` off the event loop, then refresh.
    CleanWorktree(std::path::PathBuf),
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
}

impl MonitorTab {
    pub const ALL: [MonitorTab; 8] = [
        MonitorTab::Cpu,
        MonitorTab::Memory,
        MonitorTab::Thermal,
        MonitorTab::Network,
        MonitorTab::Disk,
        MonitorTab::Gpu,
        MonitorTab::Power,
        MonitorTab::Procs,
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
            _ => None,
        }
    }

    /// Whether this machine has anything to show on the tab. A tab with no data
    /// is worse than a missing one: it reads as broken.
    fn present(self, s: &StatsSnapshot) -> bool {
        match self {
            MonitorTab::Gpu => s.gpu_pct.is_some(),
            MonitorTab::Power => s.battery.is_some(),
            MonitorTab::Thermal => s.cpu_temp_c.is_some() || !s.temps.is_empty(),
            MonitorTab::Disk => !s.disks.is_empty(),
            MonitorTab::Network => s.net_bps.is_some() || !s.net_ifaces.is_empty(),
            // CPU, Memory and Processes are always meaningful.
            _ => true,
        }
    }

    pub fn visible(s: &StatsSnapshot) -> Vec<MonitorTab> {
        MonitorTab::ALL
            .into_iter()
            .filter(|t| t.present(s))
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
    /// Row cursor for the list tabs (Processes and Disk).
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
    disk_rows: Vec<std::path::PathBuf>,
    /// A pending y/n confirmation (signal or clean); owns the footer while set.
    confirm: Option<Confirm>,
    /// The pid we last SIGTERM'd, so a second signal on the same process offers
    /// SIGKILL as a distinct escalation.
    last_termed: Option<u32>,
    /// A transient footer note (signal outcome, filter echo).
    status: Option<String>,
    /// An action for the loop to perform (clean); drained via [`Self::take_action`].
    pending_action: Option<MonitorAction>,
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
        let tabs = MonitorTab::visible(&model.stats);
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
            confirm: None,
            last_termed: None,
            status: None,
            pending_action: None,
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

    pub fn prefs(&self) -> &MonitorPrefs {
        &self.prefs
    }

    /// Drain a pending loop-side action (clean), if any. Called by the loop
    /// after every key so the overlay never touches the DB or spawns work.
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
        // Recompute the list-tab rows FIRST so `sel`, the signal action, and the
        // clean action all index exactly what the renderer draws.
        self.proc_rows = procs_view::rows(&model.procs, self.proc_view());
        self.disk_rows = build::worktree_disk_rows(model, now / 1000);
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
            disk_eta: ctx.hist.disk_fill_eta(),
        });
        self.body = body;
        self.covered_secs = ctx.hist.coverage_secs(now, self.prefs.tab(self.tab).window);
        self.clamp();
    }

    /// Keep the row cursor inside the current list tab's rows. The list shrinks
    /// under the user (a process exits, a worktree is cleaned), and a stranded
    /// cursor would act on the wrong row or none.
    fn clamp_sel(&mut self) {
        let len = match self.tab {
            MonitorTab::Procs => self.proc_rows.len(),
            MonitorTab::Disk => self.disk_rows.len(),
            _ => 0,
        };
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
        self.tabs = MonitorTab::visible(&model.stats);
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
/// interactive. Best-effort: a clean that fails is logged, never a crash.
pub fn spawn_clean(path: std::path::PathBuf, waker: termwiz::terminal::TerminalWaker) {
    std::thread::Builder::new()
        .name("thegn-monitor-clean".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            match thegn_core::worktree::clean_target(&path) {
                Ok(reclaimed) => {
                    // Drop the stale badge immediately; the next disk scan
                    // remeasures. best-effort: the DB is a cache.
                    if let Ok(db) = thegn_core::db::Db::open() {
                        use thegn_core::store::WorkspaceStore;
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
        .ok();
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
        // Two sub-modes own every key while active, so a `/` filter or a signal
        // confirmation is never half-swallowed by the global handlers below.
        if self.filtering {
            return self.filter_key(key);
        }
        if self.confirm.is_some() {
            return self.confirm_key(key);
        }
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => MonitorOutcome::Close,
                _ => MonitorOutcome::Pending,
            };
        }
        // Alt/Super chords belong to the compositor, not to us.
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return MonitorOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return MonitorOutcome::Close;
        }
        let shift = mods.contains(Modifiers::SHIFT);
        let page = self.body_rows.saturating_sub(1).max(1) as isize;

        match key {
            KeyCode::Char('q') => MonitorOutcome::Close,

            // --- Tabs ---
            KeyCode::Tab if shift => {
                self.switch(-1);
                MonitorOutcome::Pending
            }
            KeyCode::Tab | KeyCode::Char('\t') => {
                self.switch(1);
                MonitorOutcome::Pending
            }
            KeyCode::RightArrow | KeyCode::Char('l') => {
                self.switch(1);
                MonitorOutcome::Pending
            }
            KeyCode::LeftArrow | KeyCode::Char('h') => {
                self.switch(-1);
                MonitorOutcome::Pending
            }
            // Digits index the VISIBLE tabs, so `2` means the same thing on a
            // laptop and a GPU-less server. Out of range is a no-op.
            KeyCode::Char(c @ '1'..='9') => {
                let i = (*c as usize) - ('1' as usize);
                if let Some(t) = self.tabs.get(i).copied() {
                    self.tab = t;
                    self.sel = 0;
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
            _ => MonitorOutcome::Pending,
        }
    }

    /// Scroll, or move the row cursor on a list tab (Processes and Disk).
    fn nav(&mut self, delta: isize) {
        if matches!(self.tab, MonitorTab::Procs | MonitorTab::Disk) {
            let len = match self.tab {
                MonitorTab::Procs => self.proc_rows.len(),
                _ => self.disk_rows.len(),
            };
            let max = len.saturating_sub(1) as isize;
            self.sel = (self.sel as isize + delta).clamp(0, max.max(0)) as usize;
        }
        self.scroll_by(delta);
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
        let Some(path) = self.disk_rows.get(self.sel).cloned() else {
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
