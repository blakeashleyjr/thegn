//! Network tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::sections::{Cell, GraphSection, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

pub(in crate::monitor) fn network(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let rx = cx.series(Metric::NetRx);
    let tx = cx.series(Metric::NetTx);
    let mut out = vec![Section::Graph(GraphSection {
        label: "NET".into(),
        cur: format!(
            "↓{} ↑{}",
            Unit::BytesPerSec.fmt(rx.last),
            Unit::BytesPerSec.fmt(tx.last)
        ),
        footer: Some("↓ rx (top) · ↑ tx (bottom)".into()),
        series: rx.hi.clone(),
        lo: None,
        axis: Vec::new(),
        tone: Tok::Hue(Hue::Green),
        height: MAIN_H,
        series2: Some((tx.hi.clone(), Tok::Hue(Hue::Blue))),
        style: cx.prefs.style,
    })];

    out.push(spacer());
    let down = s
        .net_ifaces
        .iter()
        .filter(|iface| iface.up == Some(false))
        .count();
    let note = if down == 0 {
        format!("{}", s.net_ifaces.len())
    } else {
        format!("{}, {down} down", s.net_ifaces.len())
    };
    out.push(heading("interfaces", Some(note)));
    if s.net_ifaces.is_empty() {
        out.push(Section::KeyVal(vec![kv(
            "interfaces",
            "idle".into(),
            Tok::Slot(S::Ghost),
        )]));
    } else {
        let rows: Vec<Vec<Cell>> = s
            .net_ifaces
            .iter()
            .map(|iface| {
                let ghost = iface.up == Some(false) || (iface.rx_bps == 0 && iface.tx_bps == 0);
                let tone = if ghost {
                    Tok::Slot(S::Ghost)
                } else {
                    Tok::Slot(S::Text)
                };
                vec![
                    Cell::Text(trunc(&iface.name, 16), tone),
                    Cell::Text(iface.kind.label().into(), Tok::Slot(S::Ghost)),
                    Cell::Text(
                        match iface.up {
                            Some(true) => "up",
                            Some(false) => "down",
                            None => "",
                        }
                        .into(),
                        tone,
                    ),
                    Cell::Text(
                        format!("↓{}", Unit::BytesPerSec.fmt(iface.rx_bps as f32)),
                        if ghost { tone } else { Tok::Hue(Hue::Green) },
                    ),
                    Cell::Text(
                        format!("↑{}", Unit::BytesPerSec.fmt(iface.tx_bps as f32)),
                        if ghost { tone } else { Tok::Hue(Hue::Blue) },
                    ),
                ]
            })
            .collect();
        out.push(Section::Table(TableSection {
            header: vec![
                "iface".into(),
                "kind".into(),
                "state".into(),
                "rx".into(),
                "tx".into(),
            ],
            rows,
            sel: None,
        }));
    }
    if let (Some(all), Some(physical)) = (s.net_bps_all, s.net_bps)
        && all != physical
    {
        out.push(Section::KeyVal(vec![kv(
            "all links",
            format!(
                "↓{} ↑{}",
                Unit::BytesPerSec.fmt(all.0 as f32),
                Unit::BytesPerSec.fmt(all.1 as f32)
            ),
            Tok::Slot(S::Dim),
        )]));
    }
    out
}
