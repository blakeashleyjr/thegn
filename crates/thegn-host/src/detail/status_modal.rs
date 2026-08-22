//! The expanded `thegn status` dashboard opened by the far-right daemon chip: a
//! stacked-sections popup covering daemon identity + health + policy, the
//! daemon's live session table, both processes' memory/CPU trends, and the full
//! event-loop rollup. A child module of `detail` (like `ci_drill` / `usage_dash`)
//! so it reaches the private [`DetailOverlay`] fields.
//!
//! **Where the session numbers come from.** Not the lease table — that records
//! only *detached* sessions (`kind = "relay"`, deleted again on attach), so a
//! daemon busy serving panes used to report `sessions 0 (0 attached)`. The
//! daemon's in-memory registry is the source of truth; the compositor asks for
//! it over the control socket when the user opens this modal
//! (`handlers::status::probe_sessions`) and parks the answer in
//! [`DaemonSessions`]. No timer, no poll: the socket is touched exactly when a
//! human is looking, which is what keeps the 0%-idle invariant intact.

use super::{
    Cell, DetailContent, DetailOverlay, GraphSection, Placement, Section, SectionsDetail,
    TableSection, fmt_eta, plot_cols, spacer, trunc,
};
use crate::chrome::{FrameModel, S};
use crate::seg::Tok;
use thegn_core::theme::Hue;
use thegn_svc::control::SessionInfo;

/// The overlay title — also the marker [`refresh_open`] uses to recognise a
/// still-open status modal when fresh data lands.
pub(crate) const TITLE: &str = "thegn status";

/// Requested content width. The popup clamps this to the screen (see
/// [`super::sections`]), and every derived size reads the CLAMPED width back —
/// feeding a sample count derived from 88 into a plot the layer shrank to 68
/// cells would silently drop the newest samples, because `viz::braille_graph`
/// truncates rather than resamples.
const WANT_COLS: usize = 88;

/// Floor for the requested width, so a narrow terminal still gets the old
/// single-column reading rather than an unusably squeezed grid.
const MIN_COLS: usize = 44;

/// Below this width the key/value grids collapse to one column and the session
/// table sheds its optional columns.
const WIDE_AT: usize = 70;

/// Body rows the session table shows before collapsing the tail into `+N more`.
const MAX_SESSION_ROWS: usize = 6;

/// The daemon's live session list, fetched on demand over the control socket.
/// The probe states are distinct on purpose: "we never asked" must not render
/// the same as "we asked and the daemon owns nothing".
#[derive(Debug, Clone, Default, PartialEq)]
pub enum DaemonSessions {
    /// Not asked yet (or the daemon answered `/health` but not `/v1/sessions`).
    #[default]
    Unknown,
    /// A probe is in flight; the modal is already painted from cached status.
    Probing,
    /// The probe found no daemon at all — panes here run inline.
    NoDaemon,
    /// The daemon's own registry, newest last (the daemon sorts by open time).
    Live(Vec<SessionInfo>),
}

/// Human-readable byte size (RSS): "180.0M", "1.4G", "42B".
pub(crate) fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

/// Coarse duration, largest two units: "2d 3h", "4h 12m", "5m 9s", "8s".
pub(crate) fn fmt_uptime(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Microsecond duration as the tightest readable unit: "88µs", "1.2ms", "3.4s".
fn fmt_us(us: u64) -> String {
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    }
}

/// A `p50 … · p99 …` latency pair. Renders `—` when the interval saw no samples
/// (the profiler reports 0 for "never happened", which would read as "instant").
fn pair(p50: u64, p99: u64) -> String {
    if p50 == 0 && p99 == 0 {
        "—".into()
    } else {
        format!("p50 {} · p99 {}", fmt_us(p50), fmt_us(p99))
    }
}

/// One-word summary of the chip state for the modal's role line.
pub(crate) fn role_word(state: crate::chrome::DaemonChipState) -> &'static str {
    use crate::chrome::DaemonChipState::*;
    match state {
        NonPersist => "non-persistent (inline pane)",
        Persist => "persistent (daemon-backed)",
        Server => "server (serving remote clients)",
        Client => "client (remote daemon)",
    }
}

/// Liveness from the registry heartbeat: `healthy` while within the discovery
/// TTL, `stale …` past it (a daemon whose heartbeat lapsed is one clients can no
/// longer discover, even if its process is still up).
fn health_note(status: &crate::chrome::DaemonStatus, now_ms: i64) -> (String, Tok) {
    use thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS;
    if !status.present {
        return ("no daemon".into(), Tok::Slot(S::Ghost));
    }
    let age = (now_ms - status.heartbeat_at).max(0);
    let ago = fmt_uptime((age / 1000) as u64);
    if age <= DAEMON_HEARTBEAT_TTL_MS {
        (
            format!("healthy · heartbeat {ago} ago"),
            Tok::Hue(Hue::Green),
        )
    } else {
        (format!("stale · heartbeat {ago} ago"), Tok::Hue(Hue::Amber))
    }
}

/// Shorten a path for display, collapsing `$HOME` to `~`.
fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// The `[daemon]` policy cells: how long a detached PTY stays warm, when an idle
/// daemon exits, and whether new panes route through it at all. Zero means
/// "never" for both knobs, and `idle_exit_secs` is ignored in serve mode — the
/// TCP listener must outlive "no sessions yet" (`daemon::idle_exit_window`).
fn policy_cells(
    cfg: &thegn_core::config::DaemonConfig,
    status: &crate::chrome::DaemonStatus,
) -> Vec<(String, String, Tok)> {
    // Policy knobs are coarse durations, so `fmt_eta` ("30m", "1h 0m") reads
    // better here than `fmt_uptime`'s second-precision "30m 0s".
    let grace = if cfg.lease_grace_secs == 0 {
        "never reap".to_string()
    } else {
        fmt_eta(cfg.lease_grace_secs)
    };
    let idle = if !status.tcp_addr.is_empty() {
        "n/a (serve mode)".to_string()
    } else if cfg.idle_exit_secs == 0 {
        "never".to_string()
    } else {
        fmt_eta(cfg.idle_exit_secs)
    };
    let route = if cfg.enabled {
        ("panes route to daemon".to_string(), Tok::Hue(Hue::Green))
    } else {
        ("inline ([daemon] off)".to_string(), Tok::Slot(S::Ghost))
    };
    vec![
        ("lease grace".into(), grace, Tok::Slot(S::Text)),
        ("idle exit".into(), idle, Tok::Slot(S::Text)),
        ("routing".into(), route.0, route.1),
    ]
}

/// Daemon identity + endpoint, as key/value pairs for the wide grid.
fn identity_cells(
    model: &FrameModel,
    ctx: &super::StatusCtx,
    wide: bool,
) -> Vec<(String, String, Tok)> {
    let d = ctx.daemon;
    let mut kv: Vec<(String, String, Tok)> = vec![(
        "role".into(),
        role_word(model.daemon_state).into(),
        Tok::Slot(S::Text),
    )];
    if !d.present {
        kv.push((
            "daemon".into(),
            "none (inline panes only)".into(),
            Tok::Slot(S::Ghost),
        ));
        return kv;
    }
    kv.push((
        "pid".into(),
        d.pid.map_or("—".into(), |p| p.to_string()),
        Tok::Slot(S::Dim),
    ));
    if !d.version.is_empty() {
        kv.push(("version".into(), d.version.clone(), Tok::Slot(S::Dim)));
    }
    if !d.hostname.is_empty() {
        kv.push(("host".into(), d.hostname.clone(), Tok::Slot(S::Dim)));
    }
    let uptime = (ctx.now_ms - d.started_at_ms).max(0) as u64 / 1000;
    kv.push(("uptime".into(), fmt_uptime(uptime), Tok::Slot(S::Text)));
    if !d.daemon_id.is_empty() {
        // The registry handle is long; its leading bytes are enough to match
        // against a log line or `thegn session list`.
        kv.push((
            "daemon id".into(),
            trunc(d.daemon_id.clone(), 12),
            Tok::Slot(S::Dim),
        ));
    }
    kv.push((
        "transport".into(),
        if d.remote {
            "remote".into()
        } else if cfg!(windows) {
            "named pipe".into()
        } else {
            "unix socket".into()
        },
        Tok::Slot(S::Dim),
    ));
    kv.push((
        "serving".into(),
        if d.tcp_addr.is_empty() {
            "—".into()
        } else {
            d.tcp_addr.clone()
        },
        if d.tcp_addr.is_empty() {
            Tok::Slot(S::Ghost)
        } else {
            Tok::Hue(Hue::Blue)
        },
    ));
    // The endpoint and scope are long paths; at narrow widths they'd swallow a
    // grid column, so give them a generous budget only when there is room.
    let path_w = if wide { 46 } else { 30 };
    if !d.scope.is_empty() {
        kv.push((
            "scope".into(),
            trunc(tilde(&d.scope), path_w),
            Tok::Slot(S::Ghost),
        ));
    }
    if !d.endpoint.is_empty() {
        kv.push((
            "endpoint".into(),
            trunc(tilde(&d.endpoint), path_w),
            Tok::Slot(S::Ghost),
        ));
    }
    kv
}

/// The session table's heading note: `N live · N attached · N warm`, or the
/// probe state when there is no list to summarise.
/// `age_secs` is how old the live list is (the probe is re-run while the modal
/// is open, so this normally reads "just now"); past [`SESSIONS_STALE_SECS`]
/// the note says so in amber rather than letting an old table look live.
fn sessions_note(s: &DaemonSessions, age_secs: Option<u64>) -> (String, Tok) {
    match s {
        DaemonSessions::Unknown => ("unavailable".into(), Tok::Slot(S::Ghost)),
        DaemonSessions::Probing => ("probing daemon…".into(), Tok::Slot(S::Ghost)),
        DaemonSessions::NoDaemon => ("no daemon — inline panes only".into(), Tok::Slot(S::Ghost)),
        DaemonSessions::Live(v) if v.is_empty() => ("none".into(), Tok::Slot(S::Ghost)),
        DaemonSessions::Live(v) => {
            let attached: u32 = v.iter().map(|s| s.attached_clients).sum();
            let warm = v.iter().filter(|s| s.attached_clients == 0).count();
            let base = format!("{} live · {attached} attached · {warm} warm", v.len());
            match age_secs {
                Some(a) if a >= SESSIONS_STALE_SECS => (
                    format!("{base} · as of {} ago", fmt_uptime(a)),
                    Tok::Hue(Hue::Amber),
                ),
                _ => (base, Tok::Slot(S::Text)),
            }
        }
    }
}

/// Age past which the live session table is flagged as stale.
const SESSIONS_STALE_SECS: u64 = 15;

/// Whether `slot` currently holds the status modal (for the loop's
/// re-probe-while-open cadence).
pub(crate) fn is_open(slot: &Option<DetailOverlay>) -> bool {
    slot.as_ref().is_some_and(|ov| ov.title == TITLE)
}

/// One row per daemon session. `att` is the real attached-client count the
/// daemon maintains (`LiveMeta.attached`), not a lease-table guess.
fn session_rows(v: &[SessionInfo], now_ms: i64, wide: bool) -> TableSection {
    let mut header: Vec<String> = vec!["id".into(), "program".into()];
    if wide {
        header.push("worktree".into());
        header.push("size".into());
    }
    header.extend(["att".into(), "age".into(), "lease".into()]);

    let shown = v.len().min(MAX_SESSION_ROWS);
    let mut rows: Vec<Vec<Cell>> = v
        .iter()
        .take(shown)
        .map(|s| {
            let age = ((now_ms - s.created_at_ms).max(0) / 1000) as u64;
            let att_tone = if s.attached_clients > 0 {
                Tok::Hue(Hue::Green)
            } else {
                Tok::Slot(S::Ghost)
            };
            // A detached session either counts down its relay grace or, with
            // `lease_grace_secs = 0`, stays warm indefinitely.
            let lease = match (s.attached_clients, s.lease_expires_at) {
                (n, _) if n > 0 => "—".to_string(),
                (_, Some(at)) => format!("warm {}", fmt_eta(((at - now_ms).max(0) / 1000) as u64)),
                (_, None) => "warm".to_string(),
            };
            let mut row = vec![
                Cell::Text(trunc(s.id.clone(), 10), Tok::Slot(S::Text)),
                Cell::Text(trunc(s.program.clone(), 12), Tok::Slot(S::Text)),
            ];
            if wide {
                row.push(Cell::Text(
                    s.worktree
                        .as_deref()
                        .map(|w| trunc(w.rsplit('/').next().unwrap_or(w).to_string(), 18))
                        .unwrap_or_else(|| "—".into()),
                    Tok::Slot(S::Dim),
                ));
                row.push(Cell::Text(
                    format!("{}×{}", s.cols, s.rows),
                    Tok::Slot(S::Dim),
                ));
            }
            row.push(Cell::Text(s.attached_clients.to_string(), att_tone));
            row.push(Cell::Text(fmt_uptime(age), Tok::Slot(S::Dim)));
            row.push(Cell::Text(lease, Tok::Slot(S::Ghost)));
            row
        })
        .collect();
    if v.len() > shown {
        let mut more = vec![Cell::Text(
            format!("+{} more", v.len() - shown),
            Tok::Slot(S::Ghost),
        )];
        more.resize_with(header.len(), || {
            Cell::Text(String::new(), Tok::Slot(S::Ghost))
        });
        rows.push(more);
    }
    TableSection { header, rows }
}

/// The event-loop rollup, as grid cells. Every one of these is already measured
/// by `crate::perf` each interval and was previously discarded.
fn loop_cells(p: &crate::perf::PerfSnapshot) -> Vec<(String, String, Tok)> {
    let dim = Tok::Slot(S::Dim);
    let idle_tone = if p.idle_ratio >= 0.9 {
        Tok::Hue(Hue::Green)
    } else if p.idle_ratio >= 0.5 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Hue(Hue::Red)
    };
    let budget_us = crate::perf::frame_budget_us();
    let over = p.render_p50_us > budget_us;
    vec![
        (
            "render".into(),
            pair(p.render_p50_us, p.render_p99_us),
            if over { Tok::Hue(Hue::Red) } else { dim },
        ),
        ("input".into(), pair(p.input_p50_us, p.input_p99_us), dim),
        ("flush".into(), pair(p.flush_p50_us, p.flush_p99_us), dim),
        ("drain".into(), pair(p.drain_p50_us, p.drain_p99_us), dim),
        ("switch".into(), pair(p.switch_p50_us, p.switch_p99_us), dim),
        (
            "frames".into(),
            format!(
                "{:.0}/s ({:.0} pane · {:.0} full)",
                p.renders_per_s, p.pane_frames_per_s, p.full_frames_per_s
            ),
            Tok::Slot(S::Text),
        ),
        (
            "skips".into(),
            format!("{:.0}/s", p.render_skips_per_s),
            dim,
        ),
        (
            "pty".into(),
            format!(
                "{}/s · {:.0} chunks/s",
                thegn_metrics::fmt_rate(p.pty_bytes_per_s as u64).trim(),
                p.pty_chunks_per_s
            ),
            dim,
        ),
        (
            "idle".into(),
            format!("{:.1}%", p.idle_ratio * 100.0),
            idle_tone,
        ),
        (
            "render busy".into(),
            format!("{:.1}%", p.render_busy_ratio * 100.0),
            dim,
        ),
        ("budget".into(), fmt_us(budget_us), Tok::Slot(S::Ghost)),
        (
            "hot source".into(),
            format!("{} · {:.0}/s", p.hot_source, p.hot_items_per_s),
            dim,
        ),
    ]
}

/// Build the expanded status dashboard. Anchored upward from the statusbar chip
/// like the other bottom-bar detail popups, and scrollable when it outgrows the
/// screen (`sections` clamps `rows` to what the layer will actually draw).
pub(super) fn status_detail(
    model: &FrameModel,
    ctx: &super::StatusCtx,
    near: Placement,
) -> DetailOverlay {
    super::sections(
        TITLE,
        WANT_COLS,
        build_sections(model, ctx),
        near,
        ctx.screen,
    )
}

/// Rebuild the status modal in place, iff the user still has it open. Called
/// whenever fresh data lands (the session probe, the daemon snapshot, a stats
/// sample), so the table fills and the graphs animate without a new wake source
/// — every caller rides a drain that already happens. Returns `true` when it
/// repainted.
pub(crate) fn refresh_open(
    slot: &mut Option<DetailOverlay>,
    model: &FrameModel,
    ctx: &super::StatusCtx,
) -> bool {
    let Some(ov) = slot.as_mut() else {
        return false;
    };
    if ov.title != TITLE {
        return false;
    }
    let secs = build_sections(model, ctx);
    let content: usize = secs.iter().map(Section::height).sum();
    ov.content = DetailContent::Sections(SectionsDetail { sections: secs });
    ov.rows = content.min(ctx.screen.rows.saturating_sub(3)).max(1);
    // The stack can shrink under the user (a session closed); keep the viewport
    // inside the new content rather than stranding it past the end.
    ov.scroll = ov.scroll.min(ov.scroll_max());
    true
}

/// The section stack, shared by the initial build and every in-place refresh.
fn build_sections(model: &FrameModel, ctx: &super::StatusCtx) -> Vec<Section> {
    let d = ctx.daemon;
    // Read the CLAMPED width back: `sections` shrinks the request to fit the
    // screen, and every derived size (grid columns, plot samples) must agree
    // with what actually gets drawn.
    let w = WANT_COLS
        .min(ctx.screen.cols.saturating_sub(6))
        .max(MIN_COLS);
    let wide = w >= WIDE_AT;
    let gcols = if wide { 2 } else { 1 };
    let n = plot_cols(w);
    let mut secs: Vec<Section> = Vec::new();

    // --- Daemon identity, health, policy -----------------------------------
    let (note, tone) = health_note(d, ctx.now_ms);
    secs.push(Section::HeadingToned {
        label: "daemon".into(),
        note,
        tone,
    });
    // Identity and policy share ONE grid so their key columns align — two
    // adjacent grids size their columns independently and would step.
    let mut cells = identity_cells(model, ctx, wide);
    if d.present {
        cells.extend(policy_cells(ctx.daemon_cfg, d));
        cells.push((
            "panes here".into(),
            format!("{} of {}", model.daemon_panes, model.pane_count),
            Tok::Slot(S::Dim),
        ));
    }
    secs.push(Section::Grid { cols: gcols, cells });

    // --- The daemon's live session registry --------------------------------
    secs.push(spacer());
    let (snote, stone) = sessions_note(ctx.sessions, ctx.sessions_age_secs);
    secs.push(Section::HeadingToned {
        label: "sessions".into(),
        note: snote,
        tone: stone,
    });
    if let DaemonSessions::Live(v) = ctx.sessions
        && !v.is_empty()
    {
        secs.push(Section::Table(session_rows(v, ctx.now_ms, wide)));
    }

    // --- Both processes' footprint -----------------------------------------
    let (self_rss, self_cpu, daemon_rss, daemon_cpu) = ctx.hist.last_proc();
    let has_daemon_proc = d.present && d.pid.is_some();
    secs.push(spacer());
    secs.push(Section::Heading {
        label: "thegn process".into(),
        note: Some(if has_daemon_proc {
            format!(
                "up {} · thegn {} · daemon {}",
                fmt_uptime(ctx.uptime_secs),
                human_bytes(self_rss),
                human_bytes(daemon_rss)
            )
        } else {
            format!(
                "up {} · thegn {}",
                fmt_uptime(ctx.uptime_secs),
                human_bytes(self_rss)
            )
        }),
    });
    secs.push(Section::Graph(GraphSection {
        label: "RSS".into(),
        cur: human_bytes(self_rss),
        footer: Some(match d.pid {
            Some(p) => format!("pid {} · daemon pid {p} · {n} samples", std::process::id()),
            None => format!("pid {} · {n} samples", std::process::id()),
        }),
        series: ctx.hist.self_rss_series(n),
        tone: Tok::Hue(Hue::Purple),
        height: 6,
        // Stacking the daemon's RSS under our own in one block (the plot splits
        // in half) reads the comparison at a glance and costs 6 fewer rows than
        // a second graph.
        series2: has_daemon_proc.then(|| (ctx.hist.daemon_rss_series(n), Tok::Hue(Hue::Blue))),
        ..Default::default()
    }));
    secs.push(Section::Sparkrow {
        label: "cpu".into(),
        spark: ctx.hist.self_cpu_series(24),
        cur: format!("{self_cpu:.0}%"),
        tone: Tok::Hue(Hue::Teal),
    });
    if has_daemon_proc {
        secs.push(Section::Sparkrow {
            label: "daemon cpu".into(),
            spark: ctx.hist.daemon_cpu_series(24),
            cur: format!("{daemon_cpu:.0}%"),
            tone: Tok::Hue(Hue::Blue),
        });
        secs.push(Section::Sparkrow {
            label: "daemon rss".into(),
            spark: ctx.hist.daemon_rss_series(24),
            cur: human_bytes(daemon_rss),
            tone: Tok::Hue(Hue::Blue),
        });
    }

    // --- Event-loop rollup (the same data `thegn::perf` logs) --------------
    secs.push(spacer());
    if ctx.loop_perf.has_data() {
        let p = ctx.loop_perf.last();
        secs.push(Section::Heading {
            label: "loop".into(),
            note: Some(format!("{:.0} wakes/s", p.wakes_per_s)),
        });
        secs.push(Section::Graph(GraphSection {
            label: "WAKES".into(),
            cur: format!("{:.0}/s", p.wakes_per_s),
            footer: None,
            series: ctx.loop_perf.wakes_series(n),
            tone: Tok::Hue(Hue::Amber),
            height: 3,
            series2: None,
            ..Default::default()
        }));
        secs.push(Section::Grid {
            cols: gcols,
            cells: loop_cells(p),
        });
    } else {
        secs.push(Section::Heading {
            label: "loop".into(),
            note: Some("profiler off — set THEGN_PERF=1".into()),
        });
    }
    secs
}
