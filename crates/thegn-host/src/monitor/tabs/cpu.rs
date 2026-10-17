//! Cpu tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::sections::{Cell, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::{self as viz, Unit};

pub(in crate::monitor) fn cpu(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Cpu, MAIN_H, Tok::Hue(Hue::Teal))];

    // Per-core heat grid — sampled every tick today but shown nowhere in the
    // popups, which is most of the reason this tab exists.
    if !s.cpu_cores.is_empty() {
        out.push(spacer());
        out.push(heading(
            "cores",
            Some(format!("{} logical", s.cpu_cores.len())),
        ));
        out.extend(core_rows(&s.cpu_cores, cx.cols));
    }

    out.push(spacer());
    out.push(heading("load & clock", None));
    let mut cells = Vec::new();
    if let Some((one, five, fifteen)) = s.load_avg {
        let cores = s.cpu_cores.len().max(1) as f32;
        cells.push(kv("load 1m", format!("{one:.2}"), Tok::Slot(S::Text)));
        cells.push(kv("5m", format!("{five:.2}"), Tok::Slot(S::Dim)));
        cells.push(kv("15m", format!("{fifteen:.2}"), Tok::Slot(S::Dim)));
        // Per-core is the comparable number: 4.0 is saturated on a 4-core box
        // and idle on a 64-core one.
        cells.push(kv(
            "per core",
            format!("{:.2}", one / cores),
            Tok::Slot(S::Dim),
        ));
    }
    if let Some(mhz) = s.cpu_freq_mhz {
        cells.push(kv(
            "frequency",
            Unit::Megahertz.fmt(mhz as f32),
            Tok::Slot(S::Text),
        ));
    }
    if let Some(c) = s.cpu_temp_c {
        cells.push(kv("temperature", Unit::Celsius.fmt(c), temp_tone(c)));
    }
    if !cells.is_empty() {
        out.push(Section::Grid { cols: 2, cells });
    }
    if s.load_avg.is_some() {
        out.push(cx.sparkrow(Metric::Load, "load", Tok::Hue(Hue::Blue)));
    }
    out
}

/// Per-core utilization as heat-tinted bars, wrapped to the box width.
fn core_rows(cores: &[u8], cols: usize) -> Vec<Section> {
    // Each core costs `id + bar + pct`; fit as many per row as the box allows.
    const CELL: usize = 12;
    let per_row = (cols / CELL).max(1);
    let rows: Vec<Vec<Cell>> = cores
        .chunks(per_row)
        .enumerate()
        .map(|(chunk, group)| {
            let mut row = Vec::new();
            for (i, &pct) in group.iter().enumerate() {
                let id = chunk * per_row + i;
                let frac = f32::from(pct) / 100.0;
                row.push(Cell::Text(format!("{id:>3}"), Tok::Slot(S::Ghost)));
                row.push(Cell::Bar(frac, 5, Tok::Heat(viz::heat_index(frac) as u8)));
                row.push(Cell::Text(format!("{pct:>3}%"), Tok::Slot(S::Dim)));
            }
            row
        })
        .collect();
    vec![Section::Table(TableSection {
        header: Vec::new(),
        rows,
        sel: None,
    })]
}
