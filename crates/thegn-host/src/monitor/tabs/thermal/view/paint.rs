#![allow(dead_code)] // helpers are reached by ThermalView painters

use super::{SensorRow, ThermalModel, ZoneRow};
use crate::{
    caps,
    chrome::S,
    compositor::Rect,
    monitor::kit::{
        self,
        grid::{CellGrid, CellStyle},
        meter::{Meter, MeterSpec},
        table,
    },
    seg::{StyleTok, Tok},
};
use thegn_core::{
    numfmt::{NumFmt, Reading, Unit},
    theme::{RampId, StateRole},
};
use thegn_metrics::collect::model::thermal_zone::ThermalTrip;

pub(super) fn package(grid: &mut CellGrid, rect: Rect, sensor: Option<&SensorRow>) {
    let Some(sensor) = sensor else {
        put(
            grid,
            rect,
            0,
            0,
            &format!("{}  CPU package", caps::active_glyphs().emdash),
            Tok::Slot(S::Dim),
        );
        return;
    };
    let Some(temp) = sensor.temp_c else {
        put(
            grid,
            rect,
            0,
            0,
            caps::active_glyphs().emdash,
            Tok::Slot(S::Dim),
        );
        return;
    };
    let mut meter = Meter::default();
    meter.paint(
        grid,
        Rect {
            x: rect.x,
            y: rect.y,
            cols: rect.cols,
            rows: 1,
        },
        MeterSpec {
            value: (temp / sensor.crit_c.unwrap_or(100.0)).clamp(0.0, 1.0),
            warn: 0.7,
            crit: 0.9,
            ramp: RampId::Temp,
            label: false,
            fmt: NumFmt::default(),
        },
    );
    put(
        grid,
        rect,
        0,
        1,
        &format!("{}  {} C", sensor.label, temp),
        state_tok(temp, sensor.max_c, sensor.crit_c),
    );
    if let Some(crit) = sensor.crit_c {
        put(
            grid,
            rect,
            0,
            2,
            &format!("critical at {crit:.1} C"),
            Tok::Slot(S::Dim),
        );
    }
}

pub(super) fn cores(grid: &mut CellGrid, rect: Rect, cores: &[Option<f32>; 16]) {
    let present = cores.iter().filter(|temp| temp.is_some()).count();
    if present == 0 {
        put(
            grid,
            rect,
            0,
            0,
            caps::active_glyphs().emdash,
            Tok::Slot(S::Dim),
        );
        return;
    }
    let width = rect.cols.saturating_sub(2);
    let per_row = (width / 12).max(1);
    for (index, temp) in cores
        .iter()
        .enumerate()
        .filter_map(|(index, temp)| temp.map(|value| (index, value)))
    {
        let x = (index % per_row) * 12;
        let y = index / per_row;
        if y >= rect.rows {
            break;
        }
        let mut meter = Meter::default();
        let value = (temp / 100.0).clamp(0.0, 1.0);
        meter.paint(
            grid,
            Rect {
                x: rect.x + x,
                y: rect.y + y,
                cols: 8.min(rect.cols.saturating_sub(x)),
                rows: 1,
            },
            MeterSpec {
                value,
                warn: 0.7,
                crit: 0.9,
                ramp: RampId::Temp,
                label: false,
                fmt: NumFmt::default(),
            },
        );
        put(
            grid,
            rect,
            x + 8,
            y,
            &format!("{index:02} {temp:.0}C"),
            state_tok(temp, Some(80.0), Some(95.0)),
        );
    }
}

pub(super) fn sensors(grid: &mut CellGrid, rect: Rect, sensors: &[SensorRow]) {
    if sensors.is_empty() {
        put(
            grid,
            rect,
            0,
            0,
            caps::active_glyphs().emdash,
            Tok::Slot(S::Dim),
        );
        return;
    }
    for y in 0..rect.rows {
        let sensor = &sensors[y % sensors.len()];
        let Some(temp) = sensor.temp_c else {
            put(
                grid,
                rect,
                0,
                y,
                caps::active_glyphs().emdash,
                Tok::Slot(S::Dim),
            );
            continue;
        };
        let mut meter = Meter::default();
        let value = (temp / sensor.crit_c.unwrap_or(100.0)).clamp(0.0, 1.0);
        meter.paint(
            grid,
            Rect {
                x: rect.x,
                y: rect.y + y,
                cols: rect.cols,
                rows: 1,
            },
            MeterSpec {
                value,
                warn: 0.7,
                crit: 0.9,
                ramp: RampId::Temp,
                label: false,
                fmt: NumFmt::default(),
            },
        );
    }
    let limits = sensors
        .iter()
        .map(|sensor| match (sensor.max_c, sensor.crit_c) {
            (Some(max), Some(crit)) => format!("{max:.0}/{crit:.0}"),
            (Some(max), None) => format!("max {max:.0}"),
            (None, Some(crit)) => format!("crit {crit:.0}"),
            (None, None) => caps::active_glyphs().emdash.to_owned(),
        })
        .collect::<Vec<_>>();
    let mut owned_rows: Vec<Vec<table::TableCell<'_>>> = Vec::with_capacity(sensors.len());
    for (sensor, limit) in sensors.iter().zip(&limits) {
        owned_rows.push(vec![
            table::TableCell::Text(&sensor.chip),
            table::TableCell::Text(&sensor.label),
            table::TableCell::Number(
                sensor
                    .temp_c
                    .map_or(Reading::Unknown, |temp| Reading::Value(f64::from(temp))),
            ),
            table::TableCell::Text(limit),
        ]);
    }
    let row_refs = owned_rows.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let cols = [
        table::Col {
            title: "chip",
            width: 14,
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "sensor",
            width: rect.cols.saturating_sub(42).max(8),
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "temp C",
            width: 7,
            align: table::Align::Right,
            unit: Some(Unit::Temp),
        },
        table::Col {
            title: "max/crit",
            width: 12,
            align: table::Align::Right,
            unit: None,
        },
    ];
    table::draw(
        grid,
        rect,
        &table::TableSpec {
            cols: &cols,
            rows: &row_refs,
            offset: 0,
            visible: rect.rows.saturating_sub(1),
            sort: None,
            selected: None,
            fmt: kit::edge_fmt(NumFmt::default()),
        },
    );
}

pub(super) fn zones(grid: &mut CellGrid, rect: Rect, zones: &[ZoneRow]) {
    for (index, zone) in zones.iter().enumerate().take(rect.rows) {
        let trip = zone
            .trips
            .iter()
            .filter_map(|trip| {
                trip.temp_c
                    .map(|value| format!("{} {value:.0}C", trip_label(trip)))
            })
            .collect::<Vec<_>>()
            .join(" / ");
        let temp = zone.temp_c.map_or_else(
            || caps::active_glyphs().emdash.to_owned(),
            |value| format!("{value:.1} C"),
        );
        let (alarm, style) = if zone.alarm_events > 0 {
            (
                format!("  alarm {}", zone.alarm_events),
                Tok::State(StateRole::Warn),
            )
        } else {
            (String::new(), Tok::Slot(S::Text))
        };
        put(
            grid,
            rect,
            0,
            index,
            &format!("{}  {temp}  {trip}{alarm}", zone.label),
            style,
        );
    }
}

pub(super) fn gpu(grid: &mut CellGrid, rect: Rect, model: &ThermalModel) {
    let edge = model.gfx_c.or_else(|| {
        model
            .sensors
            .iter()
            .filter(|row| row.group == ChipGroup::Gpu)
            .filter_map(|row| row.temp_c)
            .max_by(f32::total_cmp)
    });
    let junction = model.hotspot_c.or_else(|| {
        model
            .sensors
            .iter()
            .filter(|row| {
                row.group == ChipGroup::Gpu && row.label.to_ascii_lowercase().contains("junction")
            })
            .filter_map(|row| row.temp_c)
            .max_by(f32::total_cmp)
    });
    put(
        grid,
        rect,
        0,
        0,
        &format!("edge  {}", format_temp(edge)),
        Tok::Slot(S::Text),
    );
    put(
        grid,
        rect,
        0,
        1,
        &format!("junction  {}", format_temp(junction)),
        Tok::Slot(S::Text),
    );
    put(
        grid,
        rect,
        0,
        2,
        &format!("soc  {}", format_temp(model.soc_c)),
        Tok::Slot(S::Text),
    );
    if let Some(power) = model.socket_w {
        put(
            grid,
            rect,
            0,
            3,
            &format!("socket power  {power:.1} W"),
            Tok::Slot(S::Dim),
        );
    }
}

fn format_temp(temp: Option<f32>) -> String {
    temp.map_or_else(
        || caps::active_glyphs().emdash.to_owned(),
        |value| format!("{value:.1} C"),
    )
}
fn trip_label(trip: &ThermalTrip) -> &'static str {
    match trip.kind {
        thegn_metrics::collect::model::thermal_zone::TripKind::Passive => "passive",
        thegn_metrics::collect::model::thermal_zone::TripKind::Active => "active",
        thegn_metrics::collect::model::thermal_zone::TripKind::Hot => "hot",
        thegn_metrics::collect::model::thermal_zone::TripKind::Critical => "critical",
        thegn_metrics::collect::model::thermal_zone::TripKind::Unknown => "trip",
    }
}
fn state_tok(temp: f32, max: Option<f32>, crit: Option<f32>) -> Tok {
    if crit.is_some_and(|limit| temp >= limit) {
        Tok::State(StateRole::Crit)
    } else if max.is_some_and(|limit| temp >= limit) {
        Tok::State(StateRole::Warn)
    } else {
        Tok::Slot(S::Text)
    }
}
fn put(grid: &mut CellGrid, rect: Rect, x: usize, y: usize, text: &str, tok: Tok) {
    if let Some(style) = StyleTok::from_tok(tok) {
        grid.view(rect).put_str(
            x,
            y,
            text,
            CellStyle {
                fg: style,
                ..CellStyle::default()
            },
        );
    }
}
