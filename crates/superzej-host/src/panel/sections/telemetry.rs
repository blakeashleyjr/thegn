//! The telemetry section: live cpu/mem/net/gpu/battery readings. Normal is a
//! labelled meter per metric, Half adds small braille history graphs, and
//! Full is the former telemetry overlay — big braille areas with an axis
//! gutter, the per-core sparkrow, and humanized net rates.

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::seg::{Line, Tok, seg, sp};

use super::{PanelRow, SectionCtx, ac, bar_segs, d, g, g2, g3, hue, t};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
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
    rows
}

/// The event-loop self-profiler sub-block: how hard the loop works (wakes/s),
/// how often it repaints, tail render latency, the dominant wake source, and
/// the idle ratio. Fed by the `szhost::perf` rollup (forced on while this
/// section is open). The live counterpart to the `szhost::perf` log.
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
        seg(d(), format!("p99 {:.1}ms", s.render_p99_us as f64 / 1000.0)),
    ])));
    rows.push(PanelRow::plain(Line::split(
        vec![sp(1), seg(g(), "hot "), seg(ac(), s.hot_source)],
        vec![seg(d(), format!("idle {:.0}%", s.idle_ratio * 100.0))],
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
            seg(
                hue(Hue::Green),
                format!("⇣{}", superzej_metrics::fmt_rate(rx)),
            ),
            seg(g(), "  "),
            seg(
                hue(Hue::Blue),
                format!("⇡{}", superzej_metrics::fmt_rate(tx)),
            ),
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
    let rate = |v: u64| superzej_metrics::fmt_rate(v).trim().to_string();
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
            seg(d(), format!(" {}", superzej_metrics::fmt_rate(io).trim())),
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
        segs.push(seg(g(), format!("  {short} ")));
        segs.push(seg(temp_tone(*c), format!("{c:.0}°")));
    }
    vec![PanelRow::plain(Line::Segs(segs))]
}

/// One row per physical disk: mount, free-% bar, medium, IO rate. Capped so a
/// machine with many mounts doesn't flood the section.
fn disk_rows(ctx: &SectionCtx, meter_w: usize) -> Vec<PanelRow> {
    let disks = &ctx.model.stats.disks;
    if disks.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![PanelRow::plain(Line::segs(vec![seg(g2(), "DISK").bold()]))];
    for disk in disks.iter().take(4) {
        let kind = match disk.kind {
            superzej_metrics::DiskKind::Ssd => "ssd",
            superzej_metrics::DiskKind::Hdd => "hdd",
            superzej_metrics::DiskKind::Unknown => "—",
        };
        let tone = if disk.free_pct <= 5 {
            hue(Hue::Red)
        } else if disk.free_pct <= 15 {
            hue(Hue::Amber)
        } else {
            hue(Hue::Teal)
        };
        let mut l = vec![
            sp(1),
            seg(d(), format!("{:<10} ", trunc(&disk.mount, 10))),
            seg(t(), format!("{:>3}% ", disk.free_pct)),
        ];
        l.extend(bar_segs(disk.free_pct as f32 / 100.0, meter_w, tone));
        let io = disk.read_bps + disk.write_bps;
        rows.push(PanelRow::plain(Line::split(
            l,
            vec![seg(
                g(),
                format!("{kind}  ⇅{}", superzej_metrics::fmt_rate(io)),
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

/// Truncate a string to `n` chars (no ellipsis — width is precious here).
fn trunc(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
