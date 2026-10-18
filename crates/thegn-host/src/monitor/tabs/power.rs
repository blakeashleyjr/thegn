//! Power tab renderer and tab-specific monitor actions.

pub(in crate::monitor) mod view;

use super::super::build::*;
use crate::chrome::S;
use crate::sections::{Section, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

pub(in crate::monitor) fn power(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let Some((pct, on_ac)) = s.battery else {
        return vec![heading("no battery on this machine", None)];
    };
    let tone = if on_ac {
        Tok::Hue(Hue::Green)
    } else if pct <= 15 {
        Tok::Hue(Hue::Red)
    } else if pct <= 30 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Hue(Hue::Blue)
    };
    let mut out = vec![cx.graph(Metric::Battery, MAIN_H, tone)];

    out.push(spacer());
    out.push(heading(
        "battery",
        Some(if on_ac {
            "on AC".into()
        } else {
            "discharging".into()
        }),
    ));
    let mut cells = vec![
        kv("charge", format!("{pct}%"), tone),
        kv(
            "source",
            if on_ac { "AC".into() } else { "battery".into() },
            Tok::Slot(S::Dim),
        ),
    ];
    if let Some(w) = s.battery_power_w {
        cells.push(kv("draw", Unit::Watts.fmt(w), Tok::Slot(S::Dim)));
    }
    if let Some(secs) = s.battery_eta_secs {
        cells.push(kv(
            if on_ac { "to full" } else { "to empty" },
            fmt_eta(secs),
            Tok::Slot(S::Text),
        ));
    }
    out.push(Section::Grid { cols: 2, cells });
    if s.battery_power_w.is_some() {
        out.push(cx.sparkrow(Metric::BatteryPower, "draw", Tok::Hue(Hue::Amber)));
    }
    out
}

fn fmt_eta(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}
