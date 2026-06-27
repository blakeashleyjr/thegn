//! Bar-item detail overlays: the popup/modal a masthead or statusbar item opens
//! when activated (Enter on the focused item, or a click). Mirrors the
//! [`crate::menu::MenuOverlay`] lifecycle — an `Option<DetailOverlay>` slot in
//! the loop, fed `handle_key`, painted last via `render` over the composed
//! frame, dismissed on Esc.
//!
//! Content comes in three shapes: a time-series [`GraphDetail`] (CPU/mem/temp/
//! load/net history, via [`crate::telemetry::TelemetryHistory`] + the braille
//! primitives in [`superzej_core::viz`]), a scrollable [`ListDetail`]
//! (notifications, tests), and a static [`KeyValDetail`] block (disk, battery,
//! swap/gpu/freq/uptime, PR, date/clock). [`open_detail_for`] maps an item id to
//! its content, size, and placement; the data is snapshotted at open time so the
//! overlay owns it (no borrow of the model held across frames).

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::{self, BarBadge, BarItemId, FrameModel, S};
use crate::compositor::Rect;
use crate::layer::{self, Anchor, LayerSpec};
use crate::seg::{self, Line, Tok, seg};
use crate::telemetry::TelemetryHistory;
use superzej_core::theme::Hue;
use superzej_core::viz;

/// A time-series graph (one or two normalized 0..=1 series). `series2`, when
/// present, splits the plot area in half (e.g. net rx over tx).
pub struct GraphDetail {
    pub label: String,
    pub cur: String,
    pub footer: String,
    pub series: Vec<f32>,
    pub tone: Tok,
    pub series2: Option<(Vec<f32>, Tok, String)>, // (values, tone, half-label)
}

/// One scrollable list row: a colored marker glyph, the body text, and an
/// optional dim right-aligned note (relative time, count, …).
pub struct DetailRow {
    pub marker: Tok,
    pub glyph: String,
    pub text: String,
    pub note: Option<String>,
}

/// A scrollable list (notifications, test failures, …).
pub struct ListDetail {
    pub rows: Vec<DetailRow>,
    pub empty_hint: String,
}

/// A static key/value block (one `key … value` row per pair).
pub struct KeyValDetail {
    pub pairs: Vec<(String, String, Tok)>,
}

/// What a detail overlay shows.
pub enum DetailContent {
    Graph(GraphDetail),
    List(ListDetail),
    KeyVal(KeyValDetail),
}

/// Where the box sits relative to the originating bar item.
#[derive(Debug, Clone, Copy)]
enum Placement {
    Center,
    NearBelow(Rect),
    NearAbove(Rect),
}

impl Placement {
    /// Pick a near-the-item placement: drop below an item in the screen's top
    /// half, float above one in the bottom half (so masthead items open
    /// downward, statusbar items upward), each clamped on-screen.
    fn near(anchor: Rect, screen: Rect) -> Placement {
        if anchor.y < screen.y + screen.rows / 2 {
            Placement::NearBelow(anchor)
        } else {
            Placement::NearAbove(anchor)
        }
    }

    /// Resolve to a layer [`Anchor`] for a `spec` on `screen`.
    fn anchor(self, spec: &LayerSpec, screen: Rect) -> Anchor {
        match self {
            Placement::Center => Anchor::Center,
            Placement::NearBelow(r) => {
                let (bw, bh) = layer::box_dims(spec, screen);
                let (x, y) = layer::clamp_origin(r.x, r.y + r.rows, bw, bh, screen);
                Anchor::At { x, y }
            }
            Placement::NearAbove(r) => {
                let (bw, bh) = layer::box_dims(spec, screen);
                let want_y = r.y.saturating_sub(bh);
                let (x, y) = layer::clamp_origin(r.x, want_y, bw, bh, screen);
                Anchor::At { x, y }
            }
        }
    }
}

/// The summoned bar-item detail overlay.
pub struct DetailOverlay {
    title: String,
    content: DetailContent,
    cols: usize,
    rows: usize,
    placement: Placement,
    /// List scroll offset (rows scrolled off the top); ignored by Graph/KeyVal.
    scroll: usize,
}

/// What a key delivered to the detail overlay meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailOutcome {
    Pending,
    Close,
}

impl DetailOverlay {
    /// Total rows in a list (0 for non-list content), for scroll clamping.
    fn list_len(&self) -> usize {
        match &self.content {
            DetailContent::List(l) => l.rows.len(),
            _ => 0,
        }
    }

    /// Esc / q / Enter / Ctrl-C / Ctrl-G close; for a list, j/k/arrows/PageUp/
    /// Down scroll (clamped); everything else is Pending.
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> DetailOutcome {
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => DetailOutcome::Close,
                _ => DetailOutcome::Pending,
            };
        }
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return DetailOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return DetailOutcome::Close;
        }
        match key {
            KeyCode::Char('q') | KeyCode::Enter => DetailOutcome::Close,
            KeyCode::DownArrow | KeyCode::Char('j') => {
                let max = self.list_len().saturating_sub(1);
                self.scroll = (self.scroll + 1).min(max);
                DetailOutcome::Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                DetailOutcome::Pending
            }
            KeyCode::PageDown => {
                let max = self.list_len().saturating_sub(1);
                self.scroll = (self.scroll + 8).min(max);
                DetailOutcome::Pending
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(8);
                DetailOutcome::Pending
            }
            _ => DetailOutcome::Pending,
        }
    }

    /// Paint the overlay as a summoned layer over the composed frame.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let mut spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(" esc ".into()),
            cols: self.cols,
            rows: self.rows,
            ..LayerSpec::default()
        };
        spec.anchor = self.placement.anchor(&spec, screen);
        let Some(inner) = layer::open_layer(surface, screen, &spec) else {
            return;
        };
        match &self.content {
            DetailContent::Graph(g) => render_graph(surface, inner, g),
            DetailContent::List(l) => render_list(surface, inner, l, self.scroll),
            DetailContent::KeyVal(kv) => render_keyval(surface, inner, kv),
        }
    }
}

fn panel() -> Tok {
    Tok::Slot(S::Panel)
}

fn render_graph(surface: &mut Surface, inner: Rect, g: &GraphDetail) {
    // Header: label (dim) … current value (toned).
    seg::draw_line(
        surface,
        inner.x,
        inner.y,
        inner.cols,
        &Line::split(
            vec![seg(Tok::Slot(S::Dim), g.label.clone())],
            vec![seg(g.tone, g.cur.clone()).bold()],
        ),
        panel(),
    );
    // Plot area sits between the header (row 0) and the footer (last row).
    let plot_top = inner.y + 1;
    let plot_h = inner.rows.saturating_sub(2);
    let w = inner.cols;
    if plot_h > 0 && w > 0 {
        match &g.series2 {
            None => draw_series(surface, inner.x, plot_top, w, plot_h, &g.series, g.tone),
            Some((s2, tone2, _)) => {
                let top_h = plot_h.div_ceil(2);
                let bot_h = plot_h - top_h;
                draw_series(surface, inner.x, plot_top, w, top_h, &g.series, g.tone);
                if bot_h > 0 {
                    draw_series(surface, inner.x, plot_top + top_h, w, bot_h, s2, *tone2);
                }
            }
        }
    }
    // Footer: min/avg/max (or a per-graph summary), ghost.
    if inner.rows >= 2 {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), g.footer.clone())]),
            panel(),
        );
    }
}

fn draw_series(
    surface: &mut Surface,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    vals: &[f32],
    tone: Tok,
) {
    for (i, row) in viz::braille_graph(vals, w, h).into_iter().enumerate() {
        seg::draw_line(
            surface,
            x,
            y + i,
            w,
            &Line::segs(vec![seg(tone, row)]),
            panel(),
        );
    }
}

fn render_list(surface: &mut Surface, inner: Rect, l: &ListDetail, scroll: usize) {
    if l.rows.is_empty() {
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), l.empty_hint.clone())]),
            panel(),
        );
        return;
    }
    let scroll = scroll.min(l.rows.len().saturating_sub(1));
    for (row, item) in l.rows.iter().skip(scroll).take(inner.rows).enumerate() {
        let mut left = vec![
            seg(item.marker, format!("{} ", item.glyph)),
            seg(Tok::Slot(S::Text), item.text.clone()),
        ];
        let line = match &item.note {
            Some(n) => Line::split(left, vec![seg(Tok::Slot(S::Ghost), n.clone())]),
            None => {
                left.push(seg(Tok::Slot(S::Text), String::new()));
                Line::segs(left)
            }
        };
        seg::draw_line(surface, inner.x, inner.y + row, inner.cols, &line, panel());
    }
}

fn render_keyval(surface: &mut Surface, inner: Rect, kv: &KeyValDetail) {
    for (row, (k, v, tone)) in kv.pairs.iter().take(inner.rows).enumerate() {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + row,
            inner.cols,
            &Line::split(
                vec![seg(Tok::Slot(S::Dim), k.clone())],
                vec![seg(*tone, v.clone())],
            ),
            panel(),
        );
    }
}

// --- content builders -------------------------------------------------------

/// min / avg / max of a 0..=1 series (empty → all zero).
fn stats01(s: &[f32]) -> (f32, f32, f32) {
    if s.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let min = s.iter().copied().fold(f32::INFINITY, f32::min);
    let max = s.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let avg = s.iter().copied().sum::<f32>() / s.len() as f32;
    (min, avg, max)
}

/// Plot width in cells → series length (two braille dot-columns per cell).
fn plot_cols(cols: usize) -> usize {
    cols.saturating_sub(0) * 2
}

/// Build the detail overlay for a focused bar item, or `None` when the item has
/// no data to show (so Enter is a no-op rather than an empty modal). `anchor` is
/// the item's on-screen rect (for popup placement); `hist` is the rolling
/// telemetry history (panel-UI docs).
pub fn open_detail_for(
    id: &BarItemId,
    anchor: Rect,
    screen: Rect,
    model: &FrameModel,
    hist: &TelemetryHistory,
) -> Option<DetailOverlay> {
    let near = Placement::near(anchor, screen);
    match id {
        BarItemId::Widget(w) => widget_detail(w, near, model, hist),
        BarItemId::Badge(b) => badge_detail(*b, near, model),
    }
}

fn graph(
    title: &str,
    label: &str,
    cur: String,
    footer: String,
    series: Vec<f32>,
    tone: Tok,
    placement: Placement,
) -> DetailOverlay {
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::Graph(GraphDetail {
            label: label.to_string(),
            cur,
            footer,
            series,
            tone,
            series2: None,
        }),
        cols: 40,
        rows: 12,
        placement,
        scroll: 0,
    }
}

fn keyval(
    title: &str,
    pairs: Vec<(String, String, Tok)>,
    cols: usize,
    placement: Placement,
) -> DetailOverlay {
    let rows = pairs.len().max(1);
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::KeyVal(KeyValDetail { pairs }),
        cols,
        rows,
        placement,
        scroll: 0,
    }
}

fn list(
    title: &str,
    rows: Vec<DetailRow>,
    empty_hint: &str,
    cols: usize,
    height: usize,
) -> DetailOverlay {
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::List(ListDetail {
            rows,
            empty_hint: empty_hint.to_string(),
        }),
        cols,
        rows: height,
        placement: Placement::Center,
        scroll: 0,
    }
}

fn widget_detail(
    w: &str,
    near: Placement,
    model: &FrameModel,
    hist: &TelemetryHistory,
) -> Option<DetailOverlay> {
    let s = &model.stats;
    let n = plot_cols(40);
    match w {
        "cpu" => {
            let series = hist.cpu_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s.cpu_pct.map_or("—".into(), |p| format!("{p}%"));
            let footer = format!(
                "min {:.0}%  avg {:.0}%  max {:.0}%",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            Some(graph(
                "CPU history",
                "CPU",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Teal),
                near,
            ))
        }
        "mem" => {
            let series = hist.mem_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s
                .mem_gib
                .map_or("—".into(), |(u, t)| format!("{u:.1}/{t:.0}G"));
            let footer = format!(
                "min {:.0}%  avg {:.0}%  max {:.0}%",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            Some(graph(
                "Memory history",
                "MEM",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Purple),
                near,
            ))
        }
        "temp" => {
            s.cpu_temp_c?;
            let series = hist.temp_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s.cpu_temp_c.map_or("—".into(), |c| format!("{c:.0}°C"));
            let footer = format!(
                "min {:.0}°C  avg {:.0}°C  max {:.0}°C",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            Some(graph(
                "Temperature history",
                "TEMP",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Amber),
                near,
            ))
        }
        "load" => {
            let (one, five, fifteen) = s.load_avg?;
            let series = hist.load_series(n);
            let cur = format!("{one:.2}");
            let footer = format!("1m {one:.2} · 5m {five:.2} · 15m {fifteen:.2}");
            Some(graph(
                "Load average",
                "LOAD",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Blue),
                near,
            ))
        }
        "net" => {
            let (rx, tx) = hist.last_rates();
            let mut ov = graph(
                "Network history",
                "NET",
                format!(
                    "↓{} ↑{}",
                    superzej_metrics::fmt_rate(rx).trim(),
                    superzej_metrics::fmt_rate(tx).trim()
                ),
                "↓ rx (top) · ↑ tx (bottom)".into(),
                hist.rx_series(n),
                Tok::Hue(Hue::Green),
                near,
            );
            if let DetailContent::Graph(g) = &mut ov.content {
                g.series2 = Some((hist.tx_series(n), Tok::Hue(Hue::Blue), "tx".into()));
            }
            ov.cols = 44;
            Some(ov)
        }
        "swap" => {
            let (u, t) = s.swap_gib?;
            Some(keyval(
                "Swap",
                vec![
                    ("used".into(), format!("{u:.1}G"), Tok::Slot(S::Text)),
                    ("total".into(), format!("{t:.0}G"), Tok::Slot(S::Dim)),
                ],
                28,
                near,
            ))
        }
        "gpu" => {
            let p = s.gpu_pct?;
            Some(keyval(
                "GPU",
                vec![("utilization".into(), format!("{p}%"), Tok::Hue(Hue::Teal))],
                28,
                near,
            ))
        }
        "freq" => {
            let mhz = s.cpu_freq_mhz?;
            Some(keyval(
                "CPU frequency",
                vec![
                    (
                        "current".into(),
                        format!("{:.2} GHz", mhz as f32 / 1000.0),
                        Tok::Slot(S::Text),
                    ),
                    (
                        "cores".into(),
                        format!("{}", s.cpu_cores.len()),
                        Tok::Slot(S::Dim),
                    ),
                ],
                30,
                near,
            ))
        }
        "uptime" => {
            let secs = s.uptime_secs?;
            let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
            Some(keyval(
                "Uptime",
                vec![("up".into(), format!("{d}d {h}h {m}m"), Tok::Slot(S::Text))],
                30,
                near,
            ))
        }
        "battery" => {
            let (p, on_ac) = s.battery?;
            Some(keyval(
                "Battery",
                vec![
                    ("charge".into(), format!("{p}%"), Tok::Slot(S::Text)),
                    (
                        "power".into(),
                        if on_ac { "AC".into() } else { "battery".into() },
                        Tok::Slot(S::Dim),
                    ),
                ],
                28,
                near,
            ))
        }
        "disk" => {
            if s.disks.is_empty() {
                return None;
            }
            let pairs: Vec<(String, String, Tok)> = s
                .disks
                .iter()
                .map(|d| {
                    let tone = if d.free_pct <= 5 {
                        Tok::Hue(Hue::Red)
                    } else if d.free_pct <= 15 {
                        Tok::Hue(Hue::Amber)
                    } else {
                        Tok::Slot(S::Text)
                    };
                    (d.mount.clone(), format!("{}% free", d.free_pct), tone)
                })
                .collect();
            Some(keyval("Disks", pairs, 48, Placement::Center))
        }
        "loc" => {
            let n = model.loc?;
            Some(keyval(
                "Lines of code",
                vec![("total".into(), format!("{n}"), Tok::Slot(S::Text))],
                24,
                near,
            ))
        }
        "date" | "clock" => {
            let now = chrono::Local::now();
            Some(keyval(
                "Date & time",
                vec![
                    (
                        "date".into(),
                        now.format("%A %B %-d, %Y").to_string(),
                        Tok::Slot(S::Text),
                    ),
                    (
                        "time".into(),
                        now.format("%H:%M:%S %Z").to_string(),
                        Tok::Slot(S::Dim),
                    ),
                ],
                34,
                near,
            ))
        }
        "pr" => {
            let pr = model.panel.pr.as_ref()?;
            Some(keyval(
                "Pull request",
                vec![
                    (
                        "number".into(),
                        format!("#{}", pr.number),
                        Tok::Slot(S::Text),
                    ),
                    ("state".into(), pr.state.clone(), Tok::Slot(S::Dim)),
                    (
                        "draft".into(),
                        if pr.is_draft {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        Tok::Slot(S::Dim),
                    ),
                ],
                50,
                Placement::Center,
            ))
        }
        "tests" => {
            let t = model.panel.tests.as_ref()?;
            Some(keyval(
                "Tests",
                vec![
                    (
                        "passed".into(),
                        format!("{}", t.passed),
                        Tok::Hue(Hue::Green),
                    ),
                    ("failed".into(), format!("{}", t.failed), Tok::Hue(Hue::Red)),
                ],
                40,
                Placement::Center,
            ))
        }
        _ => None,
    }
}

fn badge_detail(b: BarBadge, near: Placement, model: &FrameModel) -> Option<DetailOverlay> {
    match b {
        BarBadge::Notifications => {
            let mut notes: Vec<_> = model.panel.notifications.clone();
            notes.sort_by_key(|n| std::cmp::Reverse(n.created_at_ms));
            let now_ms = chrono::Local::now().timestamp_millis();
            let rows: Vec<DetailRow> = notes
                .iter()
                .map(|n| {
                    let (glyph, marker) = notif_glyph(n.kind);
                    DetailRow {
                        marker: if n.read { Tok::Slot(S::Ghost) } else { marker },
                        glyph: glyph.into(),
                        text: n.message.clone(),
                        note: Some(rel_time(now_ms - n.created_at_ms)),
                    }
                })
                .collect();
            Some(list("Notifications", rows, "no notifications", 60, 16))
        }
        BarBadge::Agent => {
            let a = model.agent_activity.as_ref()?;
            let conn = match a.conn {
                chrome::AgentConn::Online => "online",
                chrome::AgentConn::Connecting => "connecting",
                chrome::AgentConn::Exited => "offline",
                chrome::AgentConn::Error => "error",
            };
            let mut pairs = vec![
                ("connection".into(), conn.into(), Tok::Slot(S::Text)),
                (
                    "last tool".into(),
                    a.last_tool.clone().unwrap_or_else(|| "—".into()),
                    Tok::Slot(S::Dim),
                ),
                (
                    "running".into(),
                    if a.running { "yes".into() } else { "no".into() },
                    Tok::Slot(S::Dim),
                ),
            ];
            if a.context_size > 0 {
                let pct = (a.context_used * 100 / a.context_size).clamp(0, 100);
                pairs.push((
                    "context".into(),
                    format!("{pct}% ({}/{})", a.context_used, a.context_size),
                    Tok::Slot(S::Dim),
                ));
            }
            Some(keyval("Agent", pairs, 56, Placement::Center))
        }
        BarBadge::Ci => {
            if model.panel.ci_runs.is_empty() {
                return None;
            }
            use superzej_core::ci::CiState;
            let rows: Vec<DetailRow> = model
                .panel
                .ci_runs
                .iter()
                .map(|r| {
                    let (glyph, marker) = match r.state {
                        CiState::Fail => ("✗", Tok::Hue(Hue::Red)),
                        CiState::Running => ("●", Tok::Hue(Hue::Amber)),
                        CiState::Pass => ("✓", Tok::Hue(Hue::Green)),
                        _ => ("•", Tok::Slot(S::Dim)),
                    };
                    DetailRow {
                        marker,
                        glyph: glyph.into(),
                        text: r.name.clone(),
                        note: None,
                    }
                })
                .collect();
            Some(list("CI runs", rows, "no CI runs", 56, 14))
        }
        BarBadge::Ingress => {
            if model.shares.is_empty() {
                return None;
            }
            let rows: Vec<DetailRow> = model
                .shares
                .iter()
                .map(|s| {
                    let marker = if s.failed {
                        Tok::Hue(Hue::Red)
                    } else if s.public {
                        Tok::Hue(Hue::Amber)
                    } else {
                        Tok::Hue(Hue::Teal)
                    };
                    DetailRow {
                        marker,
                        glyph: "⇅".into(),
                        text: format!("port {}", s.port),
                        note: s.url.clone(),
                    }
                })
                .collect();
            Some(list("Ingress shares", rows, "no shares", 60, 12))
        }
        BarBadge::Media => {
            let m = model.panel.media.as_ref()?;
            let text = m.badge()?;
            Some(keyval(
                "Now playing",
                vec![("track".into(), text, Tok::Hue(Hue::Green))],
                50,
                near,
            ))
        }
        BarBadge::AiCost => {
            let m = model.ai_metrics.as_ref()?;
            Some(keyval(
                "Agent spend",
                vec![
                    ("agent".into(), m.agent.clone(), Tok::Slot(S::Text)),
                    (
                        "cost".into(),
                        format!("${:.2}", m.cost),
                        Tok::Hue(Hue::Teal),
                    ),
                    (
                        "tokens".into(),
                        format!("{}", m.tokens.input + m.tokens.output),
                        Tok::Slot(S::Dim),
                    ),
                ],
                44,
                Placement::Center,
            ))
        }
        BarBadge::DiskWarn => {
            let total: u64 = model
                .sidebar_status
                .disk_sizes
                .values()
                .map(|&(t, _)| t.max(0) as u64)
                .sum();
            Some(keyval(
                "Worktree disk usage",
                vec![(
                    "total".into(),
                    superzej_core::disk::human(total),
                    Tok::Hue(Hue::Amber),
                )],
                40,
                near,
            ))
        }
        BarBadge::Zoom => Some(keyval(
            "Zoom",
            vec![(
                "state".into(),
                "pane zoomed fullscreen".into(),
                Tok::Hue(Hue::Purple),
            )],
            40,
            near,
        )),
        BarBadge::Lock => Some(keyval(
            "Keybind lock",
            vec![(
                "state".into(),
                "input locked (Ctrl+g toggles)".into(),
                Tok::Hue(Hue::Amber),
            )],
            44,
            near,
        )),
        BarBadge::Sync => Some(keyval(
            "Sync panes",
            vec![(
                "state".into(),
                "broadcasting input to all panes".into(),
                Tok::Hue(Hue::Red),
            )],
            44,
            near,
        )),
    }
}

fn notif_glyph(kind: superzej_core::notification::NotificationKind) -> (&'static str, Tok) {
    use superzej_core::notification::NotificationKind as K;
    match kind {
        K::AgentFailed | K::TestFailed | K::ProcessFailed => ("✗", Tok::Hue(Hue::Red)),
        K::AgentAttention | K::Overdue => ("⚑", Tok::Hue(Hue::Amber)),
        K::AgentDone | K::ProcessExited | K::WorktreeCreated => ("✓", Tok::Hue(Hue::Green)),
        _ => ("•", Tok::Hue(Hue::Blue)),
    }
}

/// A compact relative-time string from a millisecond delta ("3m", "2h", "5d").
fn rel_time(delta_ms: i64) -> String {
    let s = (delta_ms / 1000).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: 120,
            rows: 40,
        }
    }

    fn item_at(y: usize) -> Rect {
        Rect {
            x: 80,
            y,
            cols: 8,
            rows: 1,
        }
    }

    fn model_cpu(p: u8) -> FrameModel {
        FrameModel {
            stats: superzej_metrics::StatsSnapshot {
                cpu_pct: Some(p),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn cpu_maps_to_a_graph_near_the_item() {
        let model = model_cpu(42);
        let hist = TelemetryHistory::default();
        let ov = open_detail_for(
            &BarItemId::Widget("cpu".into()),
            item_at(0),
            screen(),
            &model,
            &hist,
        )
        .expect("cpu has a detail view");
        assert!(matches!(ov.content, DetailContent::Graph(_)));
        assert_eq!((ov.cols, ov.rows), (40, 12));
        // Item in the top half → drops below.
        assert!(matches!(ov.placement, Placement::NearBelow(_)));
    }

    #[test]
    fn absent_data_yields_no_modal() {
        let model = FrameModel::default(); // no gpu, no battery, no temp
        let hist = TelemetryHistory::default();
        for id in [
            "gpu", "battery", "temp", "load", "swap", "freq", "uptime", "pr", "tests",
        ] {
            assert!(
                open_detail_for(
                    &BarItemId::Widget(id.into()),
                    item_at(0),
                    screen(),
                    &model,
                    &hist
                )
                .is_none(),
                "{id} with no data should not open a modal"
            );
        }
    }

    #[test]
    fn notifications_badge_is_a_list_even_when_empty() {
        let model = FrameModel::default();
        let ov = open_detail_for(
            &BarItemId::Badge(BarBadge::Notifications),
            item_at(39),
            screen(),
            &model,
            &TelemetryHistory::default(),
        )
        .expect("notifications always opens");
        match ov.content {
            DetailContent::List(l) => {
                assert!(l.rows.is_empty());
                assert!(!l.empty_hint.is_empty());
            }
            _ => panic!("expected a list"),
        }
    }

    #[test]
    fn statusbar_item_opens_above_itself() {
        let model = model_cpu(10);
        let ov = open_detail_for(
            &BarItemId::Widget("cpu".into()),
            item_at(39),
            screen(),
            &model,
            &TelemetryHistory::default(),
        )
        .unwrap();
        assert!(matches!(ov.placement, Placement::NearAbove(_)));
    }

    #[test]
    fn list_scroll_clamps_at_both_ends() {
        let rows: Vec<DetailRow> = (0..3)
            .map(|i| DetailRow {
                marker: Tok::Slot(S::Text),
                glyph: "•".into(),
                text: format!("row {i}"),
                note: None,
            })
            .collect();
        let mut ov = list("L", rows, "empty", 40, 10);
        // Up at the top is a no-op.
        assert_eq!(
            ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE),
            DetailOutcome::Pending
        );
        assert_eq!(ov.scroll, 0);
        // Down clamps to len-1.
        for _ in 0..10 {
            ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        }
        assert_eq!(ov.scroll, 2);
    }

    #[test]
    fn esc_and_enter_close() {
        let mut ov = keyval(
            "k",
            vec![("a".into(), "b".into(), Tok::Slot(S::Text))],
            20,
            Placement::Center,
        );
        assert_eq!(
            ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
            DetailOutcome::Close
        );
        assert_eq!(
            ov.handle_key(&KeyCode::Escape, Modifiers::NONE),
            DetailOutcome::Close
        );
        assert_eq!(
            ov.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            DetailOutcome::Close
        );
        // A graph ignores arrows (no list to scroll) but stays open.
        assert_eq!(
            ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE),
            DetailOutcome::Pending
        );
    }

    #[test]
    fn renders_without_panic_and_is_legible() {
        let model = model_cpu(55);
        let mut hist = TelemetryHistory::default();
        for i in 0..50 {
            hist.push(&superzej_metrics::StatsSnapshot {
                cpu_pct: Some((i % 100) as u8),
                ..Default::default()
            });
        }
        let ov = open_detail_for(
            &BarItemId::Widget("cpu".into()),
            item_at(0),
            screen(),
            &model,
            &hist,
        )
        .unwrap();
        let mut s = Surface::new(120, 40);
        ov.render(&mut s, screen());
        assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
    }
}
