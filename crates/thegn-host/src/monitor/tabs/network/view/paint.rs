use super::{
    HEALTH, HISTORY, IFACE_HISTORY, INTERFACES, NetworkModel, TELEMETRY_HISTORY, THROUGHPUT,
};
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
use thegn_core::theme::RampId;

const NAME_WIDTH: usize = 16;
const KIND_WIDTH: usize = 10;
const SCOPE_WIDTH: usize = 10;
const RATE_WIDTH: usize = 10;
const ERR_WIDTH: usize = 7;
const SPARK_WIDTH: usize = 17;
const SPARK_SERIES_WIDTH: usize = 8;

pub(super) fn paint(model: &NetworkModel, surface: &mut Surface, id: PanelId, rect: Rect) {
    let mut grid = CellGrid::new(rect.cols, rect.rows);
    let title = match id {
        THROUGHPUT => format!(
            "Network throughput ({} scale rx {} / tx {} B/s)",
            model.history_scale.label(),
            rate(
                (model.history_rx_axis_max.is_finite() && model.history_rx_axis_max > 0.0)
                    .then_some(f64::from(model.history_rx_axis_max))
            ),
            rate(
                (model.history_tx_axis_max.is_finite() && model.history_tx_axis_max > 0.0)
                    .then_some(f64::from(model.history_tx_axis_max))
            )
        ),
        INTERFACES => format!(
            "Interfaces - rates/errors (spark peak {} B/s)",
            if model.interfaces.iter().any(|entry| {
                entry.live && (entry.rx.has_finite_sample() || entry.tx.has_finite_sample())
            }) {
                format_rate(f64::from(interface_peak(model)))
            } else {
                caps::active_glyphs().emdash.to_owned()
            }
        ),
        HEALTH => "Network health".to_owned(),
        _ => return,
    };
    let body = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), &title, None).rect();
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
    if id == INTERFACES {
        paint_interface_sparks(model, surface, body, rect);
    } else if id == HEALTH {
        paint_health_history(model, surface, body, rect);
    }
}

fn paint_throughput(grid: &mut CellGrid, body: Rect, model: &NetworkModel) {
    if body.cols == 0 || body.rows == 0 {
        return;
    }
    let glyphs = caps::active_glyphs();
    let rx_label = format!(
        "{} rx {}",
        glyphs.arrow_down,
        reading(model.total.rx_bps, model.health, true)
    );
    let tx_label = format!(
        "{} tx {}",
        glyphs.arrow_up,
        reading(model.total.tx_bps, model.health, true)
    );
    grid.view(Rect { rows: 1, ..body })
        .put_str(0, 0, &rx_label, CellStyle::default());
    if !model.history_rx.has_finite_sample() || !model.history_tx.has_finite_sample() {
        let rows = body.rows.saturating_sub(1).min(1);
        if rows > 0 {
            let mark = health_mark(model.health);
            grid.view(Rect {
                y: body.y + 1,
                rows,
                ..body
            })
            .put_str(0, 0, mark, CellStyle::default());
        }
        return;
    }
    if body.rows > 1 {
        let label_y = body.rows.saturating_sub(1);
        grid.view(body)
            .put_str(0, label_y, &tx_label, CellStyle::default());
    }
    let graph_rows = body.rows.saturating_sub(2);
    let half = graph_rows / 2;
    if half == 0 || body.cols == 0 {
        return;
    }
    let mut rx = [0.0; TELEMETRY_HISTORY];
    let mut tx = [0.0; TELEMETRY_HISTORY];
    let rx_len = model.history_rx.copy_ordered(&mut rx);
    let tx_len = model.history_tx.copy_ordered(&mut tx);
    for value in rx.iter_mut().take(rx_len).chain(tx.iter_mut().take(tx_len)) {
        *value = if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
    let braille = crate::monitor::graphs::table().braille;
    let used = if braille {
        rx_len.max(tx_len).div_ceil(2)
    } else {
        rx_len.max(tx_len)
    };
    let graph_cols = body.cols.min(used);
    let graph_x = body.cols.saturating_sub(graph_cols);
    let mut raster = thegn_core::viz_raster::CellBuf::new();
    raster.reset(graph_cols, graph_rows);
    thegn_core::viz_raster::mirrored(
        &rx[..rx_len],
        &tx[..tx_len],
        crate::monitor::graphs::table(),
        &mut raster,
    );
    let mut canvas = grid.view(body);
    for y in 0..graph_rows {
        for x in 0..graph_cols {
            if let Some(cell) = raster.get(x, y) {
                canvas.put(
                    graph_x + x,
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
        &format!(
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:<SCOPE_WIDTH$} {:>RATE_WIDTH$} {:>RATE_WIDTH$} {:>ERR_WIDTH$} {:<SPARK_WIDTH$}",
            "name", "kind", "scope", "rx B/s", "tx B/s", "err+drop/s", "sparkline"
        ),
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
        let errors = rate(sum_pair(sample.rx_errs_ps, sample.tx_errs_ps));
        let drops = rate(sum_pair(sample.rx_drop_ps, sample.tx_drop_ps));
        let row = format!(
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:<SCOPE_WIDTH$} {:>RATE_WIDTH$} {:>RATE_WIDTH$} {:>ERR_WIDTH$} {:<SPARK_WIDTH$}",
            sample.name.as_str(),
            sample.kind.label(),
            scope(sample.scope),
            reading(sample.rx_bps, model.health, false),
            reading(sample.tx_bps, model.health, false),
            combine(errors, drops),
            ""
        );
        view.put_str(0, index + 1, &row, CellStyle::default());
    }
    let live_count = model.interfaces.iter().filter(|entry| entry.live).count();
    if model.overflow > 0 && live_count < max_rows {
        let other_name = format!("other +{}", model.overflow);
        let row = format!(
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:<SCOPE_WIDTH$} {:>RATE_WIDTH$} {:>RATE_WIDTH$} {:>ERR_WIDTH$} {:<SPARK_WIDTH$}",
            other_name,
            caps::active_glyphs().emdash,
            "mixed",
            reading(model.overflow_rx, model.health, false),
            reading(model.overflow_tx, model.health, false),
            caps::active_glyphs().emdash,
            "",
        );
        view.put_str(0, live_count + 1, &row, CellStyle::default());
    }
}

fn paint_health(grid: &mut CellGrid, body: Rect, model: &NetworkModel) {
    let mut view = grid.view(body);
    let label = match model.health {
        thegn_metrics::collect::health::Health::Absent(
            thegn_metrics::collect::health::Absent::NoPermission,
        ) => format!("{} root-only", health_mark(model.health)),
        thegn_metrics::collect::health::Health::Absent(_) => health_mark(model.health).to_owned(),
        _ => {
            let history_peak = peak(&model.error_history, &model.drop_history);
            let peak_label = if model.error_history.has_finite_sample()
                || model.drop_history.has_finite_sample()
            {
                format!("{}/s", format_rate(f64::from(history_peak)))
            } else {
                caps::active_glyphs().emdash.to_owned()
            };
            format!(
                "errors/s {}    drops/s {}    history peak {}",
                rate(model.error_total),
                rate(model.drop_total),
                peak_label
            )
        }
    };
    view.put_str(0, 0, &label, CellStyle::default());
}

fn paint_health_history(model: &NetworkModel, surface: &mut Surface, body: Rect, panel: Rect) {
    let max = peak(&model.error_history, &model.drop_history);
    if max <= 0.0 || body.rows < 2 {
        return;
    }
    for (row, ring) in [&model.error_history, &model.drop_history]
        .into_iter()
        .take(body.rows.saturating_sub(1))
        .enumerate()
    {
        paint_rate_history(
            ring,
            max,
            Rect {
                x: panel.x + body.x,
                y: panel.y + body.y + row + 1,
                cols: body.cols,
                rows: 1,
            },
            surface,
        );
    }
}

fn paint_rate_history(ring: &super::Ring, peak: f32, rect: Rect, surface: &mut Surface) {
    if !ring.has_finite_sample() {
        return;
    }
    let mut values = [0.0; HISTORY];
    let len = ring.copy_ordered(&mut values);
    for value in values.iter_mut().take(len) {
        if value.is_finite() {
            *value = (*value / peak).clamp(0.0, 1.0);
        }
    }
    crate::monitor::graphs::paint_sparkline(&values[..len], rect.cols, RampId::Rx, rect, surface);
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
        .map(format_rate)
        .unwrap_or_else(|| caps::active_glyphs().emdash.to_owned())
}

fn reading(
    value: Option<f64>,
    health: thegn_metrics::collect::health::Health,
    with_unit: bool,
) -> String {
    match value {
        Some(value) => format!(
            "{}{}",
            format_rate(value),
            if with_unit { " B/s" } else { "" }
        ),
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
            format!("{} root-only", caps::glyph(caps::Glyph::Lock))
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

fn peak(rx: &super::Ring, tx: &super::Ring) -> f32 {
    rx.values
        .iter()
        .chain(&tx.values)
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max)
}

fn interface_peak(model: &NetworkModel) -> f32 {
    model
        .interfaces
        .iter()
        .flat_map(|entry| entry.rx.values.iter().chain(&entry.tx.values))
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max)
}

fn paint_interface_sparks(model: &NetworkModel, surface: &mut Surface, body: Rect, panel: Rect) {
    let peak = interface_peak(model);
    if peak <= 0.0 || body.cols == 0 {
        return;
    }
    let max_rows = body.rows.saturating_sub(1);
    let x = panel.x + body.x + body.cols.saturating_sub(SPARK_WIDTH);
    for (row, entry) in model
        .interfaces
        .iter()
        .filter(|entry| entry.live)
        .take(max_rows)
        .enumerate()
    {
        paint_interface_spark(
            &entry.rx,
            peak,
            x,
            panel.y + body.y + row + 1,
            body.cols.min(SPARK_SERIES_WIDTH),
            surface,
        );
        paint_interface_spark(
            &entry.tx,
            peak,
            x + SPARK_SERIES_WIDTH + 1,
            panel.y + body.y + row + 1,
            body.cols
                .saturating_sub(SPARK_SERIES_WIDTH + 1)
                .min(SPARK_SERIES_WIDTH),
            surface,
        );
    }
}

fn paint_interface_spark(
    ring: &super::Ring,
    peak: f32,
    x: usize,
    y: usize,
    available_cols: usize,
    surface: &mut Surface,
) {
    let mut values = [0.0; IFACE_HISTORY];
    let len = ring.copy_ordered(&mut values);
    for value in values.iter_mut().take(len) {
        *value = if value.is_finite() {
            (*value / peak).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
    crate::monitor::graphs::paint_sparkline(
        &values[..len],
        SPARK_SERIES_WIDTH,
        RampId::Rx,
        Rect {
            x,
            y,
            cols: SPARK_SERIES_WIDTH.min(available_cols),
            rows: 1,
        },
        surface,
    );
}
