use super::{HISTORY_CAP, SensorRow, ThermalModel, ZoneRow};
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
use thegn_metrics::collect::model::{hwmon::ChipGroup, thermal_zone::ThermalTrip};

const SENSOR_COLUMN_WIDTHS: [usize; 5] = [14, 22, 7, 12, 12];
pub(super) const SENSOR_HISTORY_X: usize = SENSOR_COLUMN_WIDTHS[0]
    + SENSOR_COLUMN_WIDTHS[1]
    + SENSOR_COLUMN_WIDTHS[2]
    + SENSOR_COLUMN_WIDTHS[3]
    + SENSOR_COLUMN_WIDTHS[4]
    + 5;

pub(super) fn package(
    grid: &mut CellGrid,
    rect: Rect,
    sensor: Option<&SensorRow>,
    package_history: &[f32; HISTORY_CAP],
    package_history_len: usize,
    package_history_next: usize,
) {
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
    if let Some(limit) = sensor.crit_c.or(sensor.max_c) {
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
                value: (temp / limit).clamp(0.0, 1.0),
                warn: 0.7,
                crit: 0.9,
                ramp: RampId::Temp,
                label: false,
                fmt: NumFmt::default(),
            },
        );
    }
    put(
        grid,
        rect,
        0,
        1,
        &format!("{}  {temp:.1} C", sensor.label),
        state_tok(temp, sensor.max_c, sensor.crit_c),
    );
    if let Some(limit) = sensor.crit_c.or(sensor.max_c) {
        let label = if let Some(crit) = sensor.crit_c {
            format!("scale {limit:.1} C, critical at {crit:.1} C")
        } else {
            format!("scale {limit:.1} C")
        };
        put(grid, rect, 0, 2, &label, Tok::Slot(S::Dim));
    }
    let mut frame_values = [0.0_f32; HISTORY_CAP];
    let count = package_history_len.min(HISTORY_CAP);
    let start = if package_history_len == HISTORY_CAP {
        package_history_next
    } else {
        0
    };
    for (offset, value) in frame_values.iter_mut().take(count).enumerate() {
        *value = package_history[(start + offset) % HISTORY_CAP];
    }
    let frame_history = &frame_values[..count];
    if !frame_history.is_empty() {
        put(grid, rect, 0, 3, "live 80/95 C", Tok::Slot(S::Dim));
        let graph_width = rect
            .cols
            .saturating_sub(12)
            .min(frame_history.len())
            .min(HISTORY_CAP);
        let mut values = [0.0_f32; HISTORY_CAP];
        let first = frame_history.len().saturating_sub(graph_width);
        for (index, value) in values.iter_mut().take(graph_width).enumerate() {
            *value = thermal_level(frame_history[first + index], 80.0, 95.0);
        }
        let mut graph = crate::monitor::graphs::GraphOut::new();
        crate::monitor::graphs::paint_heatmap(
            &mut graph,
            &[&values[..graph_width]],
            RampId::Temp,
            &crate::chrome::palette_snapshot(),
        );
        draw_graph(
            grid,
            rect,
            rect.cols.saturating_sub(graph_width),
            3,
            &graph.lines,
            1,
        );
    }
}

pub(super) fn telemetry_label(grid: &mut CellGrid, rect: Rect) {
    put(grid, rect, 0, 4, "telemetry 80/95 C", Tok::Slot(S::Dim));
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
    let cell_width = 10;
    let per_row = (width / cell_width).max(1);
    let palette = crate::chrome::palette_snapshot();
    for (index, temp) in cores
        .iter()
        .enumerate()
        .filter_map(|(index, temp)| temp.map(|value| (index, value)))
    {
        let x = (index % per_row) * cell_width;
        let y = index / per_row;
        if y >= rect.rows {
            break;
        }
        let normalized = thermal_level(temp, 80.0, 95.0);
        let row = [normalized];
        let mut heatmap = crate::monitor::graphs::GraphOut::new();
        crate::monitor::graphs::paint_heatmap(&mut heatmap, &[&row], RampId::Temp, &palette);
        if let Some(crate::seg::Line::Segs(segs)) = heatmap.lines.first() {
            let mut offset = 0;
            for seg in segs {
                for ch in seg.text.chars() {
                    grid.view(rect).put(
                        x + offset,
                        y,
                        CellStyle {
                            ch,
                            fg: StyleTok::from_tok(Tok::Ramp(
                                RampId::Temp,
                                (normalized * 7.0).round() as u8,
                            ))
                            .unwrap_or(StyleTok::INHERIT),
                            bg: seg
                                .bg
                                .and_then(StyleTok::from_tok)
                                .unwrap_or(StyleTok::INHERIT),
                            ..CellStyle::default()
                        },
                    );
                    offset += 1;
                }
            }
        }
        put(
            grid,
            rect,
            x + 2,
            y,
            &format!("{index:02} {temp:.0}C"),
            state_tok(temp, Some(80.0), Some(95.0)),
        );
    }
}

fn draw_graph(
    grid: &mut CellGrid,
    rect: Rect,
    x: usize,
    y: usize,
    lines: &[crate::seg::Line],
    max_rows: usize,
) {
    for (row, line) in lines.iter().enumerate().take(max_rows) {
        if let crate::seg::Line::Segs(segs) = line {
            let mut col = x;
            for seg in segs {
                for ch in seg.text.chars() {
                    grid.view(rect).put(
                        col,
                        y + row,
                        CellStyle {
                            ch,
                            fg: StyleTok::from_tok(seg.fg).unwrap_or(StyleTok::INHERIT),
                            bg: seg
                                .bg
                                .and_then(StyleTok::from_tok)
                                .unwrap_or(StyleTok::INHERIT),
                            ..CellStyle::default()
                        },
                    );
                    col += 1;
                }
            }
        }
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
    let limits = sensors
        .iter()
        .map(|sensor| match (sensor.max_c, sensor.crit_c) {
            (Some(max), Some(crit)) => format!("{max:.0}/{crit:.0}"),
            (Some(max), None) => format!("max {max:.0}"),
            (None, Some(crit)) => format!("crit {crit:.0}"),
            (None, None) => caps::active_glyphs().emdash.to_owned(),
        })
        .collect::<Vec<_>>();
    let bar_width = SENSOR_COLUMN_WIDTHS[4];
    let bars = sensors
        .iter()
        .map(
            |sensor| match (sensor.temp_c, sensor.crit_c.or(sensor.max_c)) {
                (Some(temp), Some(limit)) => {
                    let (filled, track) =
                        caps::bar_track((temp / limit).clamp(0.0, 1.0), bar_width);
                    format!("{filled}{track}")
                }
                _ => caps::active_glyphs().emdash.to_owned(),
            },
        )
        .collect::<Vec<_>>();
    let mut owned_rows: Vec<Vec<table::TableCell<'_>>> = Vec::with_capacity(sensors.len());
    for ((sensor, limit), bar) in sensors.iter().zip(&limits).zip(&bars) {
        owned_rows.push(vec![
            table::TableCell::Text(&sensor.chip),
            table::TableCell::Text(&sensor.label),
            table::TableCell::Number(
                sensor
                    .temp_c
                    .map_or(Reading::Unknown, |temp| Reading::Value(f64::from(temp))),
            ),
            table::TableCell::Text(limit),
            table::TableCell::Text(bar),
            table::TableCell::Text(""),
        ]);
    }
    let row_refs = owned_rows.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let spark_width = rect.cols.saturating_sub(SENSOR_HISTORY_X).max(1);
    let cols = [
        table::Col {
            title: "chip",
            width: SENSOR_COLUMN_WIDTHS[0],
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "sensor",
            width: SENSOR_COLUMN_WIDTHS[1],
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "temp C",
            width: SENSOR_COLUMN_WIDTHS[2],
            align: table::Align::Right,
            unit: Some(Unit::Temp),
        },
        table::Col {
            title: "max/crit",
            width: SENSOR_COLUMN_WIDTHS[3],
            align: table::Align::Right,
            unit: None,
        },
        table::Col {
            title: "to limit",
            width: bar_width,
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "history",
            width: spark_width,
            align: table::Align::Left,
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
    if zones.is_empty() {
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
    let edge = model.edge_c.or(model.gfx_c).or_else(|| {
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

pub(super) fn gpu_history_label(grid: &mut CellGrid, rect: Rect) {
    put(grid, rect, 0, 4, "GPU history 80/95 C", Tok::Slot(S::Dim));
}

pub(super) fn heatmap(
    grid: &mut CellGrid,
    rect: Rect,
    history: &[[f32; HISTORY_CAP]; 16],
    history_len: &[usize; 16],
    history_next: &[usize; 16],
) {
    if history_len.iter().all(|len| *len == 0) {
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
    let braille = crate::monitor::graphs::table().braille;
    let core_count = history_len
        .iter()
        .rposition(|len| *len > 0)
        .map_or(0, |index| index + 1);
    let raster_rows = if braille {
        core_count.div_ceil(2)
    } else {
        core_count
    };
    let label_width = if braille {
        "c00/01 ".len()
    } else {
        "core 15 ".len()
    };
    let graph_width = rect.cols.saturating_sub(label_width);
    let display_width = graph_width.min(HISTORY_CAP);
    let mut values = [[f32::NAN; HISTORY_CAP]; 16];
    for core in 0..core_count {
        let count = history_len[core].min(display_width);
        if count == 0 {
            continue;
        }
        let start = (history_next[core] + HISTORY_CAP - count) % HISTORY_CAP;
        for offset in 0..count {
            let value = history[core][(start + offset) % HISTORY_CAP];
            values[core][display_width - count + offset] = thermal_level(value, 80.0, 95.0);
        }
    }
    let rows: [&[f32]; 16] = std::array::from_fn(|core| {
        if core >= core_count || history_len[core] == 0 {
            &[]
        } else {
            &values[core][..display_width]
        }
    });
    let mut graph = crate::monitor::graphs::GraphOut::new();
    crate::monitor::graphs::paint_heatmap(
        &mut graph,
        &rows[..core_count],
        RampId::Temp,
        &crate::chrome::palette_snapshot(),
    );
    let rows_emitted = raster_rows.min(rect.rows);
    for index in 0..rows_emitted {
        let label = if braille {
            format!("c{:02}/{:02} ", index * 2, index * 2 + 1)
        } else {
            format!("core {index:02} ")
        };
        put(grid, rect, 0, index, &label, Tok::Slot(S::Dim));
    }
    draw_graph(
        grid,
        rect,
        label_width + graph_width.saturating_sub(display_width),
        0,
        &graph.lines,
        rows_emitted,
    );
}

pub(super) fn history_into(sensor: &SensorRow, width: usize, output: &mut [f32]) -> usize {
    let count = sensor.history_len.min(width).min(output.len());
    if count == 0 {
        return 0;
    }
    let first = (sensor.history_next + HISTORY_CAP - count) % HISTORY_CAP;
    for (offset, value) in output[..count].iter_mut().enumerate() {
        *value = sensor.history[(first + offset) % HISTORY_CAP];
    }
    count
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

pub(super) fn thermal_level(temp: f32, warn: f32, crit: f32) -> f32 {
    if temp <= warn {
        (temp / warn * 0.7).clamp(0.0, 0.7)
    } else {
        (0.7 + (temp - warn) / (crit - warn) * 0.2).clamp(0.7, 1.0)
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
