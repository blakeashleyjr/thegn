//! Memory tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::chrome::S;
use crate::sections::{Section, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

pub(in crate::monitor) mod view;

pub(in crate::monitor) fn memory(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Mem, MAIN_H, Tok::Hue(Hue::Purple))];

    out.push(spacer());
    out.push(heading("breakdown", None));
    let mut cells = Vec::new();
    if let Some((u, t)) = s.mem_gib {
        cells.push(kv("used", format!("{u:.1}G"), Tok::Slot(S::Text)));
        cells.push(kv(
            "available",
            format!("{:.1}G", (t - u).max(0.0)),
            Tok::Slot(S::Dim),
        ));
        if let Some(free) = s.mem_free_gib {
            cells.push(kv("free", format!("{free:.1}G"), Tok::Slot(S::Dim)));
        }
        cells.push(kv("total", format!("{t:.0}G"), Tok::Slot(S::Dim)));
        cells.push(kv(
            "in use",
            format!("{:.0}%", if t > 0.0 { u / t * 100.0 } else { 0.0 }),
            Tok::Slot(S::Dim),
        ));
    }
    if !cells.is_empty() {
        out.push(Section::Grid { cols: 2, cells });
    }

    if let Some((u, t)) = s.swap_gib {
        out.push(spacer());
        out.push(heading("swap", Some(format!("{u:.1}G of {t:.0}G"))));
        out.push(cx.graph(Metric::Swap, SUB_H, Tok::Hue(Hue::Blue)));
    }

    // thegn's own footprint — the honest place to notice the UI leaking.
    out.push(spacer());
    out.push(heading("thegn", None));
    let (rss, cpu_pct, drss, dcpu) = cx.hist.last_proc();
    let mut cells = vec![
        kv("rss", Unit::Bytes.fmt(rss as f32), Tok::Slot(S::Text)),
        kv("cpu", Unit::Percent.fmt(cpu_pct), Tok::Slot(S::Dim)),
    ];
    if drss > 0 {
        cells.push(kv(
            "daemon rss",
            Unit::Bytes.fmt(drss as f32),
            Tok::Slot(S::Dim),
        ));
        cells.push(kv("daemon cpu", Unit::Percent.fmt(dcpu), Tok::Slot(S::Dim)));
    }
    out.push(Section::Grid { cols: 2, cells });
    out.push(cx.sparkrow(Metric::SelfRss, "rss", Tok::Hue(Hue::Green)));
    out
}
