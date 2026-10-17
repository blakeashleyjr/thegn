//! Thermal tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::chrome::S;
use crate::sections::{Cell, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

pub(in crate::monitor) fn thermal(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = Vec::new();
    if s.cpu_temp_c.is_some() {
        out.push(cx.graph(Metric::Temp, MAIN_H, Tok::Hue(Hue::Amber)));
    }
    // EVERY sensor, not just the hottest — the popup only ever showed one, so
    // a hot NVMe or chipset was invisible.
    if !s.temps.is_empty() {
        out.push(spacer());
        out.push(heading("sensors", Some(format!("{}", s.temps.len()))));
        let rows: Vec<Vec<Cell>> = s
            .temps
            .iter()
            .map(|(name, c)| {
                vec![
                    Cell::Text(trunc(name, 24), Tok::Slot(S::Text)),
                    Cell::Bar((c / 100.0).clamp(0.0, 1.0), 10, temp_tone(*c)),
                    Cell::Text(Unit::Celsius.fmt(*c), temp_tone(*c)),
                ]
            })
            .collect();
        out.push(Section::Table(TableSection {
            header: vec!["sensor".into(), "".into(), "temp".into()],
            rows,
            sel: None,
        }));
    }
    if let Some(c) = s.gpu_temp_c {
        out.push(spacer());
        out.push(Section::KeyVal(vec![kv(
            "gpu",
            Unit::Celsius.fmt(c),
            temp_tone(c),
        )]));
    }
    if out.is_empty() {
        out.push(heading("no temperature sensors on this machine", None));
    }
    out
}
