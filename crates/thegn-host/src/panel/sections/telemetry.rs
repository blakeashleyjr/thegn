//! The telemetry section: live cpu/mem/net/gpu/battery readings. Normal is a
//! labelled meter per metric, Half adds small braille history graphs, and
//! Full is the former telemetry overlay — big braille areas with an axis
//! gutter, the per-core sparkrow, and humanized net rates.

use thegn_core::pipeline_report::display_cell;
use thegn_core::remote_health::HealthSource;
use thegn_core::theme::Hue;
use thegn_core::viz;
use unicode_width::UnicodeWidthChar;

use crate::seg::{Line, Tok, seg, sp};

use super::{PanelRow, SectionCtx, ac, bar_segs, d, g, g2, g3, hue, t};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows = all_rows(ctx);
    rows.into_iter().skip(ctx.ui.scroll).collect()
}

pub(super) fn row_count(ctx: &SectionCtx) -> usize {
    all_rows(ctx).len()
}

fn all_rows(ctx: &SectionCtx) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    if s.cpu_pct.is_none() && s.mem_gib.is_none() && s.net_bps.is_none() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            g(),
            "sampling system stats…",
        )]))];
    }
    let mut rows = if ctx.full() {
        full(ctx)
    } else if ctx.deep() {
        half(ctx)
    } else {
        normal(ctx)
    };
    rows.extend(loop_block(ctx));
    let key = focused_worktree_path(ctx);
    let reading = key.and_then(|k| thegn_core::remote_health::global().reading(k));
    rows.extend(remote_block(
        ctx.cols,
        ctx.deep(),
        reading.as_ref(),
        now_ms(),
    ));
    rows
}

/// The focused worktree's path (the health cache key), if the tab is known.
fn focused_worktree_path<'a>(ctx: &'a SectionCtx) -> Option<&'a str> {
    ctx.model
        .sidebar_db_worktrees
        .iter()
        .find(|w| w.tab_name == ctx.model.worktree)
        .map(|w| w.path.as_str())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A sparkline `w` cells wide of the newest `w` values, padded on the left
/// with the lowest block so a short history keeps its right edge.
fn spark_row(series: &[f32], w: usize) -> String {
    let tail = &series[series.len().saturating_sub(w)..];
    let mut vals = vec![0.0; w - tail.len()];
    vals.extend_from_slice(tail);
    viz::sparkline(&vals)
}

/// The REMOTE sub-block: the focused worktree's sandbox health with live
/// CPU / memory / disk sparklines from the sampler's ring. Empty when the
/// worktree has no sandbox reading (a local worktree shows nothing).
fn remote_block(
    cols: usize,
    deep: bool,
    reading: Option<&thegn_core::remote_health::HealthReading>,
    now_ms: i64,
) -> Vec<PanelRow> {
    use thegn_core::remote_health::{HealthLevel, short_duration};
    let Some(r) = reading else {
        return Vec::new();
    };
    let (tone, word) = match r.level() {
        HealthLevel::Ok => (hue(Hue::Green), "ok"),
        HealthLevel::Warn => (hue(Hue::Amber), "warn"),
        HealthLevel::Unknown => (g(), "unknown"),
    };
    let mut rows = vec![
        PanelRow::blank(),
        PanelRow::plain(Line::split(
            vec![seg(g2(), "REMOTE").bold()],
            vec![seg(tone, word)],
        )),
    ];
    if let Some(note) = r.note() {
        let clip = thegn_core::pipeline_report::display_clip(
            &display_cell(note),
            cols.saturating_sub(2).max(8),
        );
        rows.push(PanelRow::plain(Line::segs(vec![sp(1), seg(tone, clip)])));
    }
    let Some(sample) = r.sample.as_ref() else {
        return rows;
    };
    let gw = cols.saturating_sub(if deep { 18 } else { 20 }).clamp(6, 40);
    let pct = |p: Option<u64>| p.map_or_else(|| " -".to_string(), |p| format!("{p:>3}%"));
    let cpu_pct = sample.load.zip(sample.cpus).map(|(l, c)| {
        if c == 0 {
            0
        } else {
            (l[0] / f64::from(c) * 100.0).round() as u64
        }
    });
    let mut metric = |label: &str, value: String, series: Vec<f32>, tone: Tok| {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(g2(), format!("{label} ")).bold(),
            seg(t(), format!("{value} ")),
            seg(tone, spark_row(&series, gw)),
        ])));
    };
    metric("CPU", pct(cpu_pct), r.cpu_series(), hue(Hue::Teal));
    metric(
        "MEM",
        pct(sample.memory_percent()),
        r.mem_series(),
        hue(Hue::Purple),
    );
    metric(
        "DSK",
        pct(sample.disk_percent()),
        r.disk_series(),
        hue(Hue::Amber),
    );
    if let Some((used, limit)) = sample.memory() {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(
                g(),
                format!(
                    "{}/{} mem",
                    thegn_core::remote_health::gb(used),
                    thegn_core::remote_health::gb(limit)
                ),
            ),
        ])));
    }
    let mut facts = Vec::new();
    if let Some(up) = sample.uptime_secs {
        facts.push(format!("up {}", short_duration(up)));
    }
    if let Some(left) = r.lifetime_left_at(now_ms) {
        facts.push(format!("{} left", short_duration(left)));
    }
    if !facts.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(d(), facts.join(" · ")),
        ])));
    }
    rows
}

/// The event-loop self-profiler sub-block: how hard the loop works (wakes/s),
/// how often it repaints, tail render latency, the dominant wake source, and
/// the idle ratio. Fed by the `thegn::perf` rollup (forced on while this
/// section is open). The live counterpart to the `thegn::perf` log.
fn loop_block(ctx: &SectionCtx) -> Vec<PanelRow> {
    let h = &ctx.ui.docs.loop_perf;
    let mut rows = vec![
        PanelRow::blank(),
        PanelRow::plain(Line::segs(vec![seg(g2(), "LOOP").bold()])),
    ];
    if !h.has_data() {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(g(), "profiling… (rollup every 1s)"),
        ])));
        return rows;
    }
    let s = h.last();
    rows.push(PanelRow::plain(Line::segs(vec![
        sp(1),
        seg(t(), format!("{:.0} wake/s", s.wakes_per_s)),
        seg(g(), "  "),
        seg(t(), format!("{:.0} rend/s", s.renders_per_s)),
        seg(g(), "  "),
        seg(d(), loop_render_tail(s)),
    ])));
    rows.push(PanelRow::plain(Line::split(
        vec![sp(1), seg(g(), "hot "), seg(ac(), s.hot_source)],
        vec![seg(d(), format!("loop idle {:.0}%", s.idle_ratio * 100.0))],
    )));
    if ctx.deep() {
        let gw = ctx.cols.saturating_sub(8).clamp(12, 64);
        for row in viz::braille_graph(&h.wakes_series(gw * 2), gw, 2) {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(hue(Hue::Amber), row),
            ])));
        }
    }
    rows
}

/// The LOOP row's render tail. Reads the window's sample count, never a zero percentile (zero is also the
/// histogram's 0–1 µs bucket): `no samples`, the count and `(sparse)` below `TAIL_MIN_SAMPLES`, else p95 and p99.
fn loop_render_tail(s: &crate::perf::PerfSnapshot) -> String {
    let ms = |us: u64| us as f64 / 1000.0;
    if s.frame_samples == 0 {
        "no samples".into()
    } else if s.frame_samples < crate::perf::TAIL_MIN_SAMPLES {
        format!(
            "p99 {:.1}ms · n={} (sparse)",
            ms(s.render_p99_us),
            s.frame_samples
        )
    } else {
        format!(
            "p95 {:.1}ms · p99 {:.1}ms",
            ms(s.render_p95_us),
            ms(s.render_p99_us)
        )
    }
}

/// Normal: one labelled meter row per metric.
fn normal(ctx: &SectionCtx) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    let mut rows: Vec<PanelRow> = Vec::new();
    let meter_w = ctx.cols.saturating_sub(14).clamp(8, 24);
    if let Some(c) = s.cpu_pct {
        let mut l = vec![seg(g2(), "CPU ").bold(), seg(t(), format!("{c:>3}%  "))];
        l.extend(bar_segs(c as f32 / 100.0, meter_w, hue(Hue::Teal)));
        rows.push(PanelRow::plain(Line::segs(l)));
    }
    if let Some((used, total)) = s.mem_gib
        && total > 0.0
    {
        let mut l = vec![
            seg(g2(), "MEM ").bold(),
            seg(t(), format!("{:>3}%  ", ((used / total) * 100.0).round())),
        ];
        l.extend(bar_segs(used / total, meter_w, hue(Hue::Purple)));
        rows.push(PanelRow::plain(Line::split(
            l,
            vec![seg(g(), format!("{used:.1}/{total:.0}G"))],
        )));
    }
    if let Some(g_pct) = s.gpu_pct {
        let mut l = vec![seg(g2(), "GPU ").bold(), seg(t(), format!("{g_pct:>3}%  "))];
        l.extend(bar_segs(g_pct as f32 / 100.0, meter_w, hue(Hue::Green)));
        rows.push(PanelRow::plain(Line::segs(l)));
    }
    if let Some(c) = s.cpu_temp_c {
        let mut l = vec![seg(g2(), "TMP ").bold(), seg(t(), format!("{c:>3.0}°C "))];
        l.extend(bar_segs((c / 100.0).clamp(0.0, 1.0), meter_w, temp_tone(c)));
        rows.push(PanelRow::plain(Line::segs(l)));
    }
    if let Some((rx, tx)) = s.net_bps {
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g2(), "NET ").bold(),
            seg(hue(Hue::Green), format!("⇣{}", thegn_metrics::fmt_rate(rx))),
            seg(g(), "  "),
            seg(hue(Hue::Blue), format!("⇡{}", thegn_metrics::fmt_rate(tx))),
        ])));
    }
    rows.extend(battery_row(ctx));
    rows.extend(disk_rows(ctx, meter_w));
    rows.extend(sys_line(ctx));
    rows
}

/// Half: small braille history per metric + the core sparkrow.
fn half(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (s, h) = (&ctx.model.stats, &ctx.ui.docs.telemetry);
    let mut rows: Vec<PanelRow> = Vec::new();
    let gw = ctx.cols.saturating_sub(8).clamp(12, 64);

    rows.push(PanelRow::plain(Line::split(
        vec![seg(g2(), "CPU").bold()],
        vec![seg(d(), format!("{}%", s.cpu_pct.unwrap_or(0))).bold()],
    )));
    for row in viz::braille_graph(&h.cpu_series(gw * 2), gw, 3) {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(hue(Hue::Teal), row),
        ])));
    }
    rows.extend(core_sparkrow(&s.cpu_cores));
    rows.push(PanelRow::blank());

    let (used, total) = s.mem_gib.unwrap_or((0.0, 0.0));
    rows.push(PanelRow::plain(Line::split(
        vec![seg(g2(), "MEM").bold()],
        vec![seg(d(), format!("{used:.1}/{total:.0}G"))],
    )));
    for row in viz::braille_graph(&h.mem_series(gw * 2), gw, 2) {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(hue(Hue::Purple), row),
        ])));
    }
    rows.push(PanelRow::blank());

    // TEMP: 2-row heat braille + per-sensor line, when a sensor exists.
    if let Some(c) = s.cpu_temp_c {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::plain(Line::split(
            vec![seg(g2(), "TEMP").bold()],
            vec![seg(d(), format!("{c:.0}°C"))],
        )));
        for row in viz::braille_graph(&h.temp_series(gw * 2), gw, 2) {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(temp_tone(c), row),
            ])));
        }
        rows.extend(temps_line(ctx));
    }
    rows.push(PanelRow::blank());

    rows.push(net_row(ctx, 12));
    rows.extend(io_load_row(ctx, 12));
    rows.extend(battery_row(ctx));
    rows.extend(disk_rows(ctx, (gw / 3).clamp(8, 24)));
    rows.extend(sys_line(ctx));
    rows
}

/// Full: the former telemetry overlay layout at band width.
fn full(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (s, h) = (&ctx.model.stats, &ctx.ui.docs.telemetry);
    let gw = ctx.cols.saturating_sub(14).clamp(20, 110);
    let mut rows: Vec<PanelRow> = Vec::new();

    // CPU: headline + 5-row braille area (top teal, mid accent, low ghost).
    let cores = &s.cpu_cores;
    let cur = s.cpu_pct.unwrap_or(0);
    let mut head = vec![seg(g2(), "CPU").bold()];
    if !cores.is_empty() {
        head.push(seg(g3(), format!(" · {} cores", cores.len())));
    }
    rows.push(PanelRow::plain(Line::split(
        head,
        vec![seg(d(), format!("{cur}%")).bold()],
    )));
    let axis = ["100", "", " 50", "", "  0"];
    for (i, row) in viz::braille_graph(&h.cpu_series(gw * 2), gw, 5)
        .into_iter()
        .enumerate()
    {
        let tone = if i < 2 {
            hue(Hue::Teal)
        } else if i < 4 {
            ac()
        } else {
            g()
        };
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(tone, row),
            sp(1),
            seg(g3(), axis[i]),
        ])));
    }
    rows.extend(core_sparkrow(cores));
    rows.push(PanelRow::blank());

    // MEM: headline + 3-row purple braille.
    let (used, total) = s.mem_gib.unwrap_or((0.0, 0.0));
    let pct = if total > 0.0 {
        (used / total * 100.0).round() as u32
    } else {
        0
    };
    rows.push(PanelRow::plain(Line::split(
        vec![seg(g2(), "MEM").bold()],
        vec![
            seg(d(), format!("{used:.1}")).bold(),
            seg(g(), format!("/{total:.0}G · ")),
            seg(hue(Hue::Purple), format!("{pct}%")),
        ],
    )));
    for row in viz::braille_graph(&h.mem_series(gw * 2), gw, 3) {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(hue(Hue::Purple), row),
        ])));
    }
    rows.push(PanelRow::blank());

    // SWAP: headline + 2-row braille, only when swap is configured + in use.
    if let Some((su, st)) = s.swap_gib
        && st > 0.0
    {
        rows.push(PanelRow::plain(Line::split(
            vec![seg(g2(), "SWAP").bold()],
            vec![seg(d(), format!("{su:.1}/{st:.0}G"))],
        )));
        for row in viz::braille_graph(&h.swap_series(gw * 2), gw, 2) {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(hue(Hue::Blue), row),
            ])));
        }
        rows.push(PanelRow::blank());
    }

    // TEMP: headline + 3-row heat braille with an axis gutter + sensor line.
    if let Some(c) = s.cpu_temp_c {
        rows.push(PanelRow::plain(Line::split(
            vec![seg(g2(), "TEMP").bold()],
            vec![seg(d(), format!("{c:.0}°C")).bold()],
        )));
        let axis = ["100", " 50", "  0"];
        for (i, row) in viz::braille_graph(&h.temp_series(gw * 2), gw, 3)
            .into_iter()
            .enumerate()
        {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(temp_tone(c), row),
                sp(1),
                seg(g3(), axis[i]),
            ])));
        }
        rows.extend(temps_line(ctx));
        rows.push(PanelRow::blank());
    }

    rows.push(net_row(ctx, 24));
    rows.extend(io_load_row(ctx, 24));
    rows.extend(battery_row(ctx));
    rows.push(PanelRow::blank());
    rows.extend(disk_rows(ctx, (gw / 4).clamp(10, 30)));
    rows.extend(sys_line(ctx));
    rows
}

/// The c0..cN per-core sparkrow, heat-toned per core.
fn core_sparkrow(cores: &[u8]) -> Vec<PanelRow> {
    if cores.is_empty() {
        return Vec::new();
    }
    let mut segs = vec![sp(1)];
    for (i, &pct) in cores.iter().enumerate() {
        if i > 0 {
            segs.push(sp(1));
        }
        let tone = if pct > 80 {
            hue(Hue::Red)
        } else if pct > 55 {
            hue(Hue::Amber)
        } else {
            hue(Hue::Teal)
        };
        segs.push(seg(g2(), format!("c{i}")));
        segs.push(seg(
            tone,
            viz::SPARK[(pct as usize * 7 / 100).min(7)].to_string(),
        ));
    }
    vec![PanelRow::plain(Line::Segs(segs))]
}

/// NET: down red / up green sparklines with humanized rates.
fn net_row(ctx: &SectionCtx, spark_w: usize) -> PanelRow {
    let h = &ctx.ui.docs.telemetry;
    let (rx, tx) = h.last_rates();
    let rate = |v: u64| thegn_metrics::fmt_rate(v).trim().to_string();
    PanelRow::plain(Line::split(
        vec![
            seg(g2(), "NET").bold(),
            sp(2),
            seg(hue(Hue::Red), "⇣ "),
            seg(hue(Hue::Red), viz::sparkline(&h.rx_series(spark_w))),
            seg(d(), format!(" {}", rate(rx))),
        ],
        vec![
            seg(hue(Hue::Green), "⇡ "),
            seg(hue(Hue::Green), viz::sparkline(&h.tx_series(spark_w))),
            seg(d(), format!(" {}", rate(tx))),
        ],
    ))
}

/// IO/LD: aggregate disk-IO sparkline + load-average sparkline, when either has
/// data (disks present / unix load average).
fn io_load_row(ctx: &SectionCtx, spark_w: usize) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    let h = &ctx.ui.docs.telemetry;
    if s.disks.is_empty() && s.load_avg.is_none() {
        return Vec::new();
    }
    let io = h.last_disk_io();
    let load1 = s.load_avg.map(|(o, _, _)| o).unwrap_or(0.0);
    vec![PanelRow::plain(Line::split(
        vec![
            seg(g2(), "IO ").bold(),
            seg(hue(Hue::Amber), viz::sparkline(&h.disk_io_series(spark_w))),
            seg(d(), format!(" {}", thegn_metrics::fmt_rate(io).trim())),
        ],
        vec![
            seg(g2(), "LD "),
            seg(hue(Hue::Teal), viz::sparkline(&h.load_series(spark_w))),
            seg(d(), format!(" {load1:.2}")),
        ],
    ))]
}

/// BAT: percent + charging state, when a battery exists.
fn battery_row(ctx: &SectionCtx) -> Vec<PanelRow> {
    let Some((pct, charging)) = ctx.model.stats.battery else {
        return Vec::new();
    };
    vec![PanelRow::plain(Line::segs(vec![
        seg(g2(), "BAT ").bold(),
        seg(
            if pct < 20 { hue(Hue::Red) } else { t() },
            format!("{pct:>3}%"),
        ),
        seg(g(), if charging { "  ⚡ on AC" } else { "" }),
    ]))]
}

/// Heat tone for a temperature in °C: teal < 70 ≤ amber < 85 ≤ red.
fn temp_tone(c: f32) -> Tok {
    if c >= 85.0 {
        hue(Hue::Red)
    } else if c >= 70.0 {
        hue(Hue::Amber)
    } else {
        hue(Hue::Teal)
    }
}

/// TEMP headline + compact per-sensor list (`cpu 55° gpu 48° nvme 38°`).
fn temps_line(ctx: &SectionCtx) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    let Some(cpu) = s.cpu_temp_c else {
        return Vec::new();
    };
    let mut segs = vec![
        seg(g2(), "TEMP ").bold(),
        seg(temp_tone(cpu), format!("{cpu:.0}°C")),
    ];
    for (label, c) in s.temps.iter().take(4) {
        let short: String = label
            .chars()
            .filter(|ch| ch.is_alphanumeric())
            .take(4)
            .collect::<String>()
            .to_ascii_lowercase();
        segs.push(seg(g(), format!("  {} ", display_cell(&short))));
        segs.push(seg(temp_tone(*c), format!("{c:.0}°")));
    }
    vec![PanelRow::plain(Line::Segs(segs))]
}

/// One row per physical disk: mount, used-% bar, medium, IO rate. Capped so a
/// machine with many mounts doesn't flood the section.
fn disk_rows(ctx: &SectionCtx, meter_w: usize) -> Vec<PanelRow> {
    let disks = &ctx.model.stats.disks;
    if disks.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![PanelRow::plain(Line::segs(vec![seg(g2(), "DISK").bold()]))];
    let mut by_mount = disks.iter().collect::<Vec<_>>();
    by_mount.sort_by_key(|disk| disk.mount.len());
    let mut names = std::collections::HashSet::new();
    for disk in by_mount
        .into_iter()
        .filter(|disk| names.insert(disk.name.as_str()))
        .take(4)
    {
        let kind = match disk.kind {
            thegn_metrics::DiskKind::Ssd => "ssd",
            thegn_metrics::DiskKind::Hdd => "hdd",
            thegn_metrics::DiskKind::Unknown => "—",
        };
        let tone = if disk.free_pct <= 5 {
            hue(Hue::Red)
        } else if disk.free_pct <= 15 {
            hue(Hue::Amber)
        } else {
            hue(Hue::Teal)
        };
        let used_pct = 100u8.saturating_sub(disk.free_pct);
        let mut l = vec![
            sp(1),
            seg(d(), format!("{} ", trunc_pad(&disk.mount, 10))),
            seg(t(), format!("{:>3}% ", used_pct)),
        ];
        l.extend(bar_segs(used_pct as f32 / 100.0, meter_w, tone));
        let io = disk.read_bps + disk.write_bps;
        rows.push(PanelRow::plain(Line::split(
            l,
            vec![seg(
                g(),
                format!("{kind}  ⇅{}", thegn_metrics::fmt_rate(io)),
            )],
        )));
    }
    rows
}

/// SYS: swap / load-average / uptime headline, each shown only when present.
fn sys_line(ctx: &SectionCtx) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    let mut segs = vec![seg(g2(), "SYS ").bold()];
    let mut any = false;
    if let Some((u, total)) = s.swap_gib
        && total > 0.0
    {
        segs.push(seg(g(), "swap "));
        segs.push(seg(t(), format!("{u:.1}/{total:.0}G")));
        any = true;
    }
    if let Some((one, five, fifteen)) = s.load_avg {
        if any {
            segs.push(seg(g(), "  "));
        }
        segs.push(seg(g(), "load "));
        segs.push(seg(t(), format!("{one:.2}·{five:.2}·{fifteen:.2}")));
        any = true;
    }
    if let Some(secs) = s.uptime_secs {
        if any {
            segs.push(seg(g(), "  "));
        }
        segs.push(seg(g(), "up "));
        segs.push(seg(t(), fmt_uptime_secs(secs)));
        any = true;
    }
    if any {
        vec![PanelRow::plain(Line::Segs(segs))]
    } else {
        Vec::new()
    }
}

/// `3d4h` / `4h12m` / `12m` — mirrors the masthead's uptime formatter.
fn fmt_uptime_secs(secs: u64) -> String {
    let (dd, hh, mm) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if dd > 0 {
        format!("{dd}d{hh}h")
    } else if hh > 0 {
        format!("{hh}h{mm}m")
    } else {
        format!("{mm}m")
    }
}

/// Left-truncate a string to the right-anchored suffix whose *display* width is
/// `<= n` cells, then right-pad with spaces to exactly `n` columns. No ellipsis
/// — width is precious in the DISK column. Uses unicode-width (the chrome layout
/// invariant): a mount with wide glyphs (e.g. `/run/media/user/ボリューム`) keeps
/// the DISK row's downstream clusters column-aligned instead of overflowing.
fn trunc_pad(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    // Walk from the tail, accumulating display width until the next glyph would
    // exceed the budget — the widest right-anchored suffix that fits in `n` cells.
    let mut used = 0usize;
    let mut from = chars.len();
    while from > 0 {
        let w = UnicodeWidthChar::width(chars[from - 1]).unwrap_or(0);
        if used + w > n {
            break;
        }
        used += w;
        from -= 1;
    }
    let mut out: String = chars[from..].iter().collect();
    // Pad by display width, not char count, to land on exactly `n` columns.
    for _ in used..n {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::{PanelUi, Section};
    use unicode_width::UnicodeWidthStr;

    fn fixture(scroll: usize) -> (FrameModel, PanelUi) {
        let mut model = FrameModel::default();
        model.stats.cpu_pct = Some(30);
        let ui = PanelUi {
            open: Section::Telemetry,
            width: PanelWidth::Full,
            scroll,
            ..Default::default()
        };
        (model, ui)
    }

    fn text(rows: &[PanelRow]) -> String {
        rows.iter()
            .map(|row| match &row.line {
                Line::Blank => String::new(),
                Line::Segs(segs) => segs
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => l
                    .iter()
                    .chain(r)
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                Line::Fill { ch, .. } => ch.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn disk_rows_dedupe_device_names_and_show_used_percent() {
        let (mut model, ui) = fixture(0);
        model.stats.disks = vec![
            thegn_metrics::DiskInfo {
                name: "device-a".into(),
                mount: "/".into(),
                free_pct: 15,
                read_bps: 0,
                write_bps: 0,
                kind: thegn_metrics::DiskKind::Ssd,
                extra_mounts: 0,
            },
            thegn_metrics::DiskInfo {
                name: "device-a".into(),
                mount: "/nix".into(),
                free_pct: 15,
                read_bps: 0,
                write_bps: 0,
                kind: thegn_metrics::DiskKind::Ssd,
                extra_mounts: 0,
            },
        ]
        .into();
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        };
        let rendered = text(&content(&ctx));
        assert_eq!(rendered.matches("85%").count(), 1, "{rendered}");
        assert!(
            rendered.contains("/          "),
            "shortest mount retained: {rendered}"
        );
        assert!(
            !rendered.contains("/nix"),
            "duplicate mount omitted: {rendered}"
        );
    }

    #[test]
    fn telemetry_scroll_drops_the_first_three_rows() {
        let (model, ui) = fixture(0);
        let all = content(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        });
        let scrolled_ui = PanelUi { scroll: 3, ..ui };
        let scrolled = content(&SectionCtx {
            model: &model,
            ui: &scrolled_ui,
            cols: 80,
            rows: 30,
        });
        let scrolled_lines = scrolled.into_iter().map(|row| row.line).collect::<Vec<_>>();
        let expected = all
            .into_iter()
            .skip(3)
            .map(|row| row.line)
            .collect::<Vec<_>>();
        assert_eq!(scrolled_lines, expected);
    }

    #[test]
    fn scroll_extent_marks_telemetry_as_nav_owner() {
        let (model, ui) = fixture(0);
        let extent = super::super::scroll_extent(Section::Telemetry, &model, &ui, 80, 12)
            .expect("Telemetry is scrollable");
        assert!(extent.owns_nav);
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 12,
        };
        let viewport = super::super::scroll_viewport(&model, &ui, 80, 12);
        assert_eq!(
            extent.max,
            row_count(&ctx).saturating_sub(viewport.saturating_sub(1))
        );
    }

    #[test]
    fn disk_free_percentage_above_one_hundred_saturates_used_percentage() {
        let (mut model, ui) = fixture(0);
        let mut disks = model.stats.disks.to_vec();
        disks.push(thegn_metrics::DiskInfo {
            name: "device-a".into(),
            mount: "/".into(),
            free_pct: 120,
            read_bps: 0,
            write_bps: 0,
            kind: thegn_metrics::DiskKind::Ssd,
            extra_mounts: 0,
        });
        model.stats.disks = disks.into();
        let rendered = text(&content(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        }));
        assert!(rendered.contains("  0%"), "{rendered}");
    }

    #[test]
    fn trunc_pad_ascii_fits_and_pads_to_width() {
        // Shorter than budget → right-padded to exactly n columns.
        let out = trunc_pad("/", 10);
        assert_eq!(out, "/         ");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 10);
        // Exactly n chars → unchanged, no padding.
        assert_eq!(trunc_pad("0123456789", 10), "0123456789");
    }

    #[test]
    fn trunc_pad_ascii_keeps_the_right_anchored_tail() {
        // Longer than budget → keeps the tail (the mount leaf), like the old
        // char-count trunc did for ASCII.
        let out = trunc_pad("/run/media/user/backup", 10);
        assert_eq!(out, "ser/backup");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 10);
    }

    #[test]
    fn trunc_pad_wide_glyphs_land_on_exact_display_width() {
        // A CJK mount leaf: 4 wide glyphs = 8 columns. Budget 10 → the whole
        // "ボリューム" (5 glyphs = 10 cols) is exactly the budget; the render must
        // be exactly 10 display cells, not 10 *chars* (which would be ~20 cols).
        let mount = "/run/media/user/ボリューム";
        let out = trunc_pad(mount, 10);
        // Display width is exactly 10 (not char count) — this is the regression:
        // the old `{:<10}` + char-count trunc produced up to ~20 columns.
        assert_eq!(
            UnicodeWidthStr::width(out.as_str()),
            10,
            "DISK mount must render at its display width, got {out:?}"
        );
        // The kept suffix is the wide leaf, right-anchored.
        assert!(out.contains('ム'), "keeps the tail: {out:?}");

        // And the full seg (mount + trailing space) is 11 columns, matching the
        // format!("{} ", ...) call site — proving downstream clusters stay aligned.
        let full = format!("{out} ");
        assert_eq!(UnicodeWidthStr::width(full.as_str()), 11);
    }

    fn loop_rows(snap: crate::perf::PerfSnapshot) -> String {
        let (model, mut ui) = fixture(0);
        ui.docs.loop_perf.push(&snap);
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        };
        text(&content(&ctx))
    }

    #[test]
    fn loop_row_says_no_samples_for_an_empty_window() {
        let rendered = loop_rows(crate::perf::PerfSnapshot::default());
        assert!(rendered.contains("no samples"), "{rendered}");
        assert!(!rendered.contains("0.0ms"), "{rendered}");
    }

    #[test]
    fn loop_row_marks_a_sparse_window_with_its_count() {
        let rendered = loop_rows(crate::perf::PerfSnapshot {
            frame_samples: 3,
            render_p99_us: 8192,
            ..Default::default()
        });
        assert!(rendered.contains("n=3 (sparse)"), "{rendered}");
        assert!(!rendered.contains("p95"), "{rendered}");
    }

    #[test]
    fn loop_row_shows_p95_and_p99_for_a_full_window() {
        let rendered = loop_rows(crate::perf::PerfSnapshot {
            frame_samples: 40,
            render_p95_us: 1024,
            render_p99_us: 8192,
            ..Default::default()
        });
        assert!(rendered.contains("p95 1.0ms · p99 8.2ms"), "{rendered}");
    }

    fn remote_text(
        cols: usize,
        deep: bool,
        r: Option<&thegn_core::remote_health::HealthReading>,
    ) -> Vec<String> {
        remote_block(cols, deep, r, 0)
            .iter()
            .map(|row| match &row.line {
                Line::Segs(v) => v.iter().map(|s| s.text.clone()).collect::<String>(),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                    let j = |v: &Vec<crate::seg::Seg>| {
                        v.iter().map(|s| s.text.clone()).collect::<String>()
                    };
                    format!("{} {}", j(l), j(r))
                }
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn remote_block_renders_sparklines_at_every_width() {
        use thegn_core::remote_health::{HealthCache, HealthSample, HealthSource};
        let cache = HealthCache::default();
        for i in 0..10u64 {
            cache.record(
                "/wt/r",
                HealthSample {
                    load: Some([i as f64 / 10.0, 0.0, 0.0]),
                    cpus: Some(2),
                    uptime_secs: Some(7_500),
                    mem_total_kib: Some(4_000_000),
                    mem_available_kib: Some(1_000_000),
                    disk_total_kib: Some(100),
                    disk_used_kib: Some(40),
                    ..Default::default()
                },
                0,
                (i == 9).then(|| "sprite restarted: likely out of memory".to_string()),
                Some(5_400),
            );
        }
        let r = cache.reading("/wt/r").unwrap();
        for cols in [10usize, 24, 40, 80, 140] {
            for deep in [false, true] {
                let lines = remote_text(cols, deep, Some(&r));
                let all = lines.join("\n");
                assert!(all.contains("REMOTE"), "{cols}: {all}");
                assert!(all.contains("warn"), "{cols}: {all}");
                assert!(all.contains("CPU") && all.contains("MEM") && all.contains("DSK"));
                assert!(all.contains("75%") && all.contains("40%"), "{cols}: {all}");
                assert!(
                    all.contains("up 2h 5m") && all.contains("1h 30m left"),
                    "{all}"
                );
                for l in &lines {
                    let w = unicode_width::UnicodeWidthStr::width(l.as_str());
                    assert!(w <= cols.max(30) + 12, "{cols}: {w} {l:?}");
                }
            }
        }
    }

    #[test]
    fn remote_block_is_empty_without_a_reading_and_says_why_on_error() {
        assert!(remote_text(40, false, None).is_empty());
        let c = thegn_core::remote_health::HealthCache::default();
        c.record_error("/wt/e", "exec refused".into(), 0);
        let r = thegn_core::remote_health::HealthSource::reading(&c, "/wt/e").unwrap();
        let all = remote_text(40, false, Some(&r)).join("\n");
        assert!(
            all.contains("unknown") && all.contains("exec refused"),
            "{all}"
        );
        assert!(!all.contains("CPU"), "no fabricated sample: {all}");
    }
}
