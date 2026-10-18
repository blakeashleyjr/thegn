use super::{HEALTH, INTERFACES, NetworkModel, THROUGHPUT};
use crate::{
    caps,
    compositor::Rect,
    monitor::{
        kit::{
            emit::Emitter,
            grid::{CellGrid, CellStyle},
            panel,
        },
        layout::PanelId,
    },
};
use termwiz::surface::Surface;

pub(super) fn paint(model: &NetworkModel, surface: &mut Surface, id: PanelId, rect: Rect) {
    let mut grid = CellGrid::new(rect.cols, rect.rows);
    let title = match id {
        THROUGHPUT => "Network throughput - bytes/s",
        INTERFACES => "Interfaces - rates and errors",
        HEALTH => "Network health",
        _ => return,
    };
    let body = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), title, None).rect();
    match id {
        THROUGHPUT => paint_throughput(&mut grid, body, model),
        INTERFACES => paint_interfaces(&mut grid, body, model),
        HEALTH => paint_health(&mut grid, body, model),
        _ => return,
    }
    Emitter::default().emit(
        &grid,
        Rect::full(rect.cols, rect.rows),
        surface,
        rect.x,
        rect.y,
    );
}

fn paint_throughput(grid: &mut CellGrid, body: Rect, model: &NetworkModel) {
    if body.cols == 0 || body.rows == 0 {
        return;
    }
    let glyphs = caps::active_glyphs();
    let rx_label = format!(
        "{} rx {} B/s",
        glyphs.arrow_down,
        reading(model.total.rx_bps, model.health)
    );
    let tx_label = format!(
        "{} tx {} B/s",
        glyphs.arrow_up,
        reading(model.total.tx_bps, model.health)
    );
    grid.view(Rect { rows: 1, ..body })
        .put_str(0, 0, &rx_label, CellStyle::default());
    if model.rx.len == 0 || model.tx.len == 0 {
        let rows = body.rows.saturating_sub(1).min(1);
        if rows > 0 {
            let mark = health_mark(model.health);
            grid.view(Rect { y: 1, rows, ..body })
                .put_str(0, 0, mark, CellStyle::default());
        }
        return;
    }
    if body.rows > 1 {
        let label_y = body.rows.saturating_sub(1);
        grid.view(Rect {
            y: label_y,
            rows: 1,
            ..body
        })
        .put_str(0, label_y, &tx_label, CellStyle::default());
    }
    let graph_rows = body.rows.saturating_sub(2);
    let half = graph_rows / 2;
    if half == 0 || body.cols == 0 {
        return;
    }
    let rx = model.rx.ordered();
    let tx = model.tx.ordered();
    let mut raster = thegn_core::viz_raster::CellBuf::new();
    raster.reset(body.cols, graph_rows);
    thegn_core::viz_raster::mirrored(&rx, &tx, crate::monitor::graphs::table(), &mut raster);
    let mut canvas = grid.view(body);
    for y in 0..graph_rows {
        for x in 0..body.cols {
            if let Some(cell) = raster.get(x, y) {
                canvas.put(
                    x,
                    y + 1,
                    CellStyle {
                        ch: cell.ch,
                        ..CellStyle::default()
                    },
                );
            }
        }
    }
}

fn paint_interfaces(grid: &mut CellGrid, body: Rect, model: &NetworkModel) {
    if body.cols == 0 || body.rows == 0 {
        return;
    }
    let mut view = grid.view(body);
    view.put_str(
        0,
        0,
        "name             kind       scope       rx B/s      tx B/s  err+drop/s history",
        CellStyle::default(),
    );
    let max_rows = body.rows.saturating_sub(1);
    for (index, entry) in model
        .interfaces
        .iter()
        .filter(|entry| entry.live)
        .take(max_rows)
        .enumerate()
    {
        let sample = &entry.sample;
        let history = format!("{} {}", spark(&entry.rx), spark(&entry.tx));
        let errors = rate(sum_pair(sample.rx_errs_ps, sample.tx_errs_ps));
        let drops = rate(sum_pair(sample.rx_drop_ps, sample.tx_drop_ps));
        let row = format!(
            "{:<16} {:<10} {:<10} {:>10} {:>10} {:>7} {}",
            sample.name.as_str(),
            sample.kind.label(),
            scope(sample.scope),
            reading(sample.rx_bps, model.health),
            reading(sample.tx_bps, model.health),
            combine(errors, drops),
            history
        );
        view.put_str(0, index + 1, &row, CellStyle::default());
    }
    if model.overflow > 0 && model.interfaces.len() < max_rows {
        let row = format!(
            "other +{}        {}          mixed      {:>10} {:>10}",
            model.overflow,
            caps::active_glyphs().emdash,
            rate(model.overflow_rx),
            rate(model.overflow_tx)
        );
        view.put_str(
            0,
            model.interfaces.iter().filter(|entry| entry.live).count() + 1,
            &row,
            CellStyle::default(),
        );
    }
}

fn paint_health(grid: &mut CellGrid, body: Rect, model: &NetworkModel) {
    let mut view = grid.view(body);
    let label = match model.health {
        thegn_metrics::collect::health::Health::Absent(_) => health_mark(model.health).to_owned(),
        _ => format!(
            "errors/s {}    drops/s {}",
            rate(model.error_total),
            rate(model.drop_total)
        ),
    };
    view.put_str(0, 0, &label, CellStyle::default());
}

fn health_mark(health: thegn_metrics::collect::health::Health) -> &'static str {
    match health {
        thegn_metrics::collect::health::Health::Warming => caps::active_glyphs().ellipsis,
        thegn_metrics::collect::health::Health::Absent(
            thegn_metrics::collect::health::Absent::NoPermission,
        ) => caps::glyph(caps::Glyph::Lock),
        _ => caps::active_glyphs().emdash,
    }
}

fn rate(value: Option<f64>) -> String {
    value
        .map(|value| format_rate(value))
        .unwrap_or_else(|| caps::active_glyphs().emdash.to_owned())
}

fn reading(value: Option<f64>, health: thegn_metrics::collect::health::Health) -> String {
    match value {
        Some(value) => format_rate(value),
        None if matches!(health, thegn_metrics::collect::health::Health::Warming) => {
            caps::active_glyphs().ellipsis.to_owned()
        }
        None if matches!(
            health,
            thegn_metrics::collect::health::Health::Absent(
                thegn_metrics::collect::health::Absent::NoPermission
            )
        ) =>
        {
            caps::glyph(caps::Glyph::Lock).to_owned()
        }
        None => caps::active_glyphs().emdash.to_owned(),
    }
}

fn format_rate(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{value:.1}")
    }
}

fn sum_pair(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn combine(errors: String, drops: String) -> String {
    match (
        errors == caps::active_glyphs().emdash,
        drops == caps::active_glyphs().emdash,
    ) {
        (true, true) => caps::active_glyphs().emdash.to_owned(),
        (false, true) => errors,
        (true, false) => drops,
        (false, false) => format!("{errors}+{drops}"),
    }
}

fn scope(scope: thegn_metrics::collect::policy::net_scope::Scope) -> &'static str {
    use thegn_metrics::collect::policy::net_scope::Scope;
    match scope {
        Scope::Physical => "physical",
        Scope::Vpn => "vpn",
        Scope::Container => "container",
        Scope::Virtual => "virtual",
        Scope::Loopback => "loopback",
    }
}

fn spark(ring: &super::Ring) -> String {
    let values = ring.ordered();
    if values.is_empty() {
        return caps::active_glyphs().ellipsis.to_owned();
    }
    let max = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max);
    values
        .iter()
        .map(|value| {
            if !value.is_finite() {
                return caps::active_glyphs().middot;
            }
            let fraction = if max > 0.0 { value / max } else { 0.0 };
            if fraction > 0.5 {
                caps::active_glyphs().bar_fill
            } else if fraction > 0.25 {
                caps::active_glyphs().bar_empty
            } else {
                " "
            }
        })
        .collect()
}
