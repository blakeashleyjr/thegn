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
pub(crate) mod state;

pub(crate) use state::MonitorPrefs;

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
}

impl MonitorTab {
    pub const ALL: [MonitorTab; 9] = [
        MonitorTab::Cpu,
        MonitorTab::Memory,
        MonitorTab::Thermal,
        MonitorTab::Network,
        MonitorTab::Disk,
        MonitorTab::Gpu,
        MonitorTab::Power,
        MonitorTab::Procs,
        MonitorTab::Containers,
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
    /// is worse than a missing one: it reads as broken. `has_containers` is
    /// `!model.containers.is_empty()` — the "a container engine is present"
    /// signal the Containers tab hides on (like GPU/Power hiding with no
    /// device).
    fn present(self, s: &StatsSnapshot, has_containers: bool) -> bool {
        match self {
            MonitorTab::Gpu => s.gpu_pct.is_some(),
            MonitorTab::Power => s.battery.is_some(),
            MonitorTab::Thermal => s.cpu_temp_c.is_some() || !s.temps.is_empty(),
            MonitorTab::Disk => !s.disks.is_empty(),
            MonitorTab::Network => s.net_bps.is_some() || !s.net_ifaces.is_empty(),
            MonitorTab::Containers => has_containers,
            // CPU, Memory and Processes are always meaningful.
            _ => true,
        }
    }

    pub fn visible(s: &StatsSnapshot, has_containers: bool) -> Vec<MonitorTab> {
        MonitorTab::ALL
            .into_iter()
            .filter(|t| t.present(s, has_containers))
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
    /// Processes / Containers row cursor.
    sel: usize,
    /// Owned + foreign container rows behind the Containers tab, cached at
    /// rebuild so a key handler can resolve `sel` without a model borrow.
    container_rows: Vec<ContainerRowMeta>,
    /// A Containers-tab row action awaiting the loop (paired with
    /// [`MonitorOutcome::Action`]).
    pending_action: Option<ContainerRequest>,
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
        let tabs = MonitorTab::visible(&model.stats, !model.containers.is_empty());
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
            container_rows: Vec::new(),
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

    pub fn prefs(&self) -> &MonitorPrefs {
        &self.prefs
    }

    /// Rebuild the active tab's body from current data.
    fn rebuild(&mut self, model: &FrameModel, ctx: &StatusCtx) {
        let live_now = ctx.now_ms.max(0) as u64;
        self.last_now_ms = live_now;
        let now = self.frozen_now_ms.unwrap_or(live_now);
        // Cache the container row identities for the key handler (ours-first, the
        // same order the builder renders), and keep the cursor inside them.
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
            self.sel = self.sel.min(self.container_rows.len().saturating_sub(1));
        }
        self.body = build::tab(
            self.tab,
            model,
            ctx,
            self.prefs.tab(self.tab),
            self.cols,
            now,
            self.prefs.proc_sort,
            self.prefs.proc_desc,
            self.sel,
        );
        self.covered_secs = ctx.hist.coverage_secs(now, self.prefs.tab(self.tab).window);
        self.clamp();
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
        self.tabs = MonitorTab::visible(&model.stats, !model.containers.is_empty());
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
        // The footer notice lasts exactly one keystroke; a pending remove stays
        // armed only across a repeated `x` (any other key disarms it, so a stray
        // keypress can never turn into a confirmed removal).
        self.notice = None;
        if !matches!(key, KeyCode::Char('x')) {
            self.remove_armed = None;
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

            // --- Per-tab (last, so it can never shadow the above) ---
            KeyCode::Char(c) if self.tab == MonitorTab::Procs => self.proc_key(*c),
            KeyCode::Enter if self.tab == MonitorTab::Containers => self.container_key('\r'),
            KeyCode::Char(c) if self.tab == MonitorTab::Containers => self.container_key(*c),
            _ => MonitorOutcome::Pending,
        }
    }

    /// Scroll, or move the row cursor on a list tab.
    fn nav(&mut self, delta: isize) {
        if matches!(self.tab, MonitorTab::Procs | MonitorTab::Containers) {
            self.sel = (self.sel as isize + delta).max(0) as usize;
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
        self.pending_action = Some(ContainerRequest {
            kind,
            name: row.name,
            backend: row.backend,
            running: row.running,
        });
        MonitorOutcome::Action
    }

    /// The loop pulls a pending Containers-tab action here (see
    /// [`MonitorOutcome::Action`]).
    pub fn take_action(&mut self) -> Option<ContainerRequest> {
        self.pending_action.take()
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

    /// The key-hint footer.
    fn footer(&self) -> Line {
        // A pending confirm / action outcome takes over the footer while set.
        if let Some(notice) = &self.notice {
            return Line::split(
                vec![seg(Tok::Slot(S::Accent), notice.clone())],
                vec![seg(Tok::Slot(S::Ghost), "q close".to_string())],
            );
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
                    " sort {}{}",
                    self.prefs.proc_sort.label(),
                    if self.prefs.proc_desc { "↓" } else { "↑" }
                ),
            ));
        }
        Line::split(left, vec![seg(Tok::Slot(S::Ghost), "q close".to_string())])
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
