//! The telemetry section: live cpu/mem/net/gpu/battery readings. Normal is a
//! labelled meter per metric, Half adds small braille history graphs, and
//! Full is the former telemetry overlay — big braille areas with an axis
//! gutter, the per-core sparkrow, and humanized net rates.

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::seg::{Line, seg, sp};

use super::{PanelRow, SectionCtx, ac, bar_segs, d, g, g2, g3, hue, t};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let s = &ctx.model.stats;
    if s.cpu_pct.is_none() && s.mem_gib.is_none() && s.net_bps.is_none() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            g(),
            "sampling system stats…",
        )]))];
    }
    if ctx.full() {
        full(ctx)
    } else if ctx.deep() {
        half(ctx)
    } else {
        normal(ctx)
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
    if let Some((rx, tx)) = s.net_bps {
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g2(), "NET ").bold(),
            seg(hue(Hue::Green), format!("⇣{}", crate::stats::fmt_rate(rx))),
            seg(g(), "  "),
            seg(hue(Hue::Blue), format!("⇡{}", crate::stats::fmt_rate(tx))),
        ])));
    }
    rows.extend(battery_row(ctx));
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

    rows.push(net_row(ctx, 12));
    rows.extend(battery_row(ctx));
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

    rows.push(net_row(ctx, 24));
    rows.extend(battery_row(ctx));
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
    let rate = |v: u64| crate::stats::fmt_rate(v).trim().to_string();
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
        seg(g(), if charging { "  ⚡ charging" } else { "" }),
    ]))]
}
