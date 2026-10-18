use super::mark;
use crate::{
    compositor::Rect,
    monitor::kit::{
        grid::CellGrid,
        panel,
        table::{self, Align, Col, TableCell, TableSpec},
    },
};
use thegn_core::numfmt::{NumFmt, Reading, Unit};
use thegn_metrics::collect::{health::Health, model::cpu_irq::CpuIrqPart};

pub(super) fn paint_irq(grid: &mut CellGrid, rect: Rect, part: &CpuIrqPart, health: Health) {
    let area = {
        let body = panel::panel(
            grid,
            Rect::full(rect.cols, rect.rows),
            "IRQ / softirq",
            None,
        );
        body.rect()
    };
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let mut values: Vec<(String, Option<f64>)> = part
        .softirq_top
        .iter()
        .filter_map(|entry| {
            entry
                .total_per_s
                .map(|rate| (format!("softirq #{:016x}", entry.name_hash), Some(rate)))
        })
        .collect();
    for (name, field) in [
        ("IPI RES", 0_u8),
        ("IPI CAL", 1),
        ("IPI TLB", 2),
        ("IPI LOC", 3),
    ] {
        let mut seen = false;
        let rate = part
            .cores
            .iter()
            .filter_map(|core| {
                let value = match field {
                    0 => core.res_per_s,
                    1 => core.cal_per_s,
                    2 => core.tlb_per_s,
                    _ => core.loc_per_s,
                };
                seen |= value.is_some();
                value
            })
            .sum::<f64>();
        values.push((name.to_owned(), seen.then_some(rate)));
    }
    let missing = if health == Health::Ok {
        Health::Ok
    } else {
        health
    };
    let cells: Vec<[TableCell<'_>; 2]> = values
        .iter()
        .map(|(name, rate)| {
            [
                TableCell::Text(name.as_str()),
                rate.map_or(TableCell::Text(mark(missing)), |value| {
                    TableCell::Number(Reading::Value(value))
                }),
            ]
        })
        .collect();
    let rows: Vec<&[TableCell<'_>]> = cells.iter().map(|row| row.as_slice()).collect();
    let columns = [
        Col {
            title: "type",
            width: area.cols.saturating_sub(12).max(1),
            align: Align::Left,
            unit: None,
        },
        Col {
            title: "rate/s",
            width: 8.min(area.cols),
            align: Align::Right,
            unit: Some(Unit::Count),
        },
    ];
    table::draw(
        grid,
        area,
        &TableSpec {
            cols: &columns,
            rows: &rows,
            offset: 0,
            visible: area.rows.saturating_sub(1),
            sort: None,
            selected: None,
            fmt: NumFmt::default(),
        },
    );
}
