//! Gpu tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::chrome::S;
use crate::sections::{Section, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

#[allow(dead_code)] // wired in a later batch (V-GPU-1b)
pub(in crate::monitor) mod view;

pub(in crate::monitor) fn gpu(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Gpu, MAIN_H, Tok::Hue(Hue::Teal))];
    out.push(spacer());
    out.push(heading("device", None));
    let mut cells = Vec::new();
    if let Some(p) = s.gpu_pct {
        cells.push(kv("utilization", format!("{p}%"), Tok::Hue(Hue::Teal)));
    }
    if let Some((u, t)) = s.gpu_mem_mib {
        let label = if s.gpu_mem_shared {
            "gpu mem (shared)"
        } else {
            "vram"
        };
        let used_label = if s.gpu_mem_shared {
            "gpu mem used"
        } else {
            "vram used"
        };
        cells.push(kv(label, format!("{u} / {t} MiB"), Tok::Slot(S::Text)));
        if t > 0 {
            cells.push(kv(
                used_label,
                format!("{:.0}%", u as f32 / t as f32 * 100.0),
                Tok::Slot(S::Dim),
            ));
        }
    }
    if let Some(c) = s.gpu_temp_c {
        cells.push(kv("temperature", Unit::Celsius.fmt(c), temp_tone(c)));
    }
    if let Some(w) = s.gpu_power_w {
        cells.push(kv("power", Unit::Watts.fmt(w), Tok::Slot(S::Dim)));
    }
    out.push(Section::Grid { cols: 2, cells });
    if s.gpu_mem_mib.is_some() {
        let label = if s.gpu_mem_shared { "gpu mem" } else { "vram" };
        out.push(cx.sparkrow(Metric::GpuMem, label, Tok::Hue(Hue::Purple)));
    }
    if s.gpu_power_w.is_some() {
        out.push(cx.sparkrow(Metric::GpuPower, "power", Tok::Hue(Hue::Amber)));
    }
    out
}

#[cfg(test)]
#[path = "gpu/gpu_view_tests.rs"]
mod view_tests;
