use super::CpuModel;
use crate::{
    caps::{self, Glyph},
    chrome::S,
    compositor::Rect,
    monitor::{
        kit::{
            emit::Emitter,
            grid::{CellGrid, CellStyle},
            meter::{Meter, MeterSpec},
            panel,
        },
        layout::PanelId,
    },
    seg::{StyleTok, Tok},
};
use std::{cell::RefCell, fmt::Write as _};
use termwiz::surface::Surface;
use thegn_core::{numfmt::NumFmt, theme::RampId};
use thegn_metrics::collect::{
    health::{Absent, Health},
    model::{cpu_freq::CpuFreqPart, cpu_sched::CpuSchedPart, cpu_stat::CpuStatPart},
};

const USAGE: PanelId = PanelId("cpu.usage");
const CORES: PanelId = PanelId("cpu.cores");
const FREQ: PanelId = PanelId("cpu.freq");
const LOAD: PanelId = PanelId("cpu.load");
const IRQ: PanelId = PanelId("cpu.irq");
const SCHED: PanelId = PanelId("cpu.sched");
const IDLE: PanelId = PanelId("cpu.idle");

#[derive(Default)]
struct CpuPaintScratch {
    grid: CellGrid,
    aux_grid: CellGrid,
    text: String,
    emitter: Emitter,
}

thread_local! {
    static PAINT_SCRATCH: RefCell<CpuPaintScratch> = RefCell::new(CpuPaintScratch::default());
}

pub(super) fn paint(model: &CpuModel, surface: &mut Surface, id: PanelId, rect: Rect) {
    PAINT_SCRATCH.with(|scratch| {
        let scratch = &mut *scratch.borrow_mut();
        scratch.grid.resize(rect.cols, rect.rows);
        scratch.grid.clear();
        match id {
            USAGE => paint_heat_panel(
                rect,
                "CPU usage",
                &model.cpu_stat,
                model.cpu_stat_health,
                true,
                scratch,
            ),
            CORES => paint_cores(&mut scratch.grid, rect, model),
            FREQ => paint_freq(
                &mut scratch.grid,
                rect,
                &model.cpu_freq,
                model.cpu_freq_health,
            ),
            LOAD => paint_load(&mut scratch.grid, rect, model, &mut scratch.aux_grid),
            IRQ => irq::paint_irq(
                &mut scratch.grid,
                rect,
                &model.cpu_irq,
                model.cpu_irq_health,
            ),
            SCHED => paint_sched(
                &mut scratch.grid,
                rect,
                &model.cpu_sched,
                model.sched_health,
            ),
            IDLE => paint_idle(&mut scratch.grid, rect, model),
            _ => return,
        }
        scratch.emitter.emit(
            &scratch.grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if id == USAGE && rect.rows > 5 && rect.cols > 2 {
            let graph_rect = Rect {
                x: rect.x + 1,
                y: rect.y + 2,
                cols: rect.cols.saturating_sub(2),
                rows: rect.rows.saturating_sub(5),
            };
            if model.cpu_history.is_empty() {
                scratch
                    .aux_grid
                    .resize(graph_rect.cols, graph_rect.rows.min(1));
                scratch.aux_grid.clear();
                scratch
                    .aux_grid
                    .view(Rect::full(graph_rect.cols, graph_rect.rows.min(1)))
                    .put_str(0, 0, mark(Health::Warming), CellStyle::default());
                scratch.emitter.emit(
                    &scratch.aux_grid,
                    Rect::full(graph_rect.cols, graph_rect.rows.min(1)),
                    surface,
                    graph_rect.x,
                    graph_rect.y,
                );
            } else {
                crate::monitor::graphs::paint_history_graph(
                    &model.cpu_history,
                    thegn_core::theme::RampId::Util,
                    Rect {
                        rows: graph_rect.rows.saturating_sub(2),
                        ..graph_rect
                    },
                    surface,
                );
            }
            let io_values = model.iowait_history.ordered();
            let steal_values = model.steal_history.ordered();
            for (values, offset) in [
                (&io_values, graph_rect.rows.saturating_sub(2)),
                (&steal_values, graph_rect.rows.saturating_sub(1)),
            ] {
                if !values.is_empty() && graph_rect.rows >= 2 {
                    scratch.aux_grid.resize(3, 1);
                    scratch.aux_grid.clear();
                    let name = if offset == graph_rect.rows.saturating_sub(2) {
                        "io"
                    } else {
                        "st"
                    };
                    scratch.aux_grid.view(Rect::full(3, 1)).put_str(
                        0,
                        0,
                        name,
                        CellStyle::default(),
                    );
                    scratch.emitter.emit(
                        &scratch.aux_grid,
                        Rect::full(3, 1),
                        surface,
                        graph_rect.x,
                        graph_rect.y + offset,
                    );
                    crate::monitor::graphs::paint_sparkline(
                        values,
                        graph_rect.cols.saturating_sub(4),
                        RampId::Util,
                        Rect {
                            x: graph_rect.x + 4,
                            y: graph_rect.y + offset,
                            cols: graph_rect.cols.saturating_sub(4),
                            rows: 1,
                        },
                        surface,
                    );
                }
            }
        }
        if id == CORES && rect.rows > 2 && rect.cols > 24 {
            paint_core_heatmap(model, rect, surface);
        }
        if id == FREQ && rect.rows > 5 {
            paint_frequency_history(model, rect, surface);
        }
        if id == LOAD && rect.rows > 5 && rect.cols > 2 && !model.load_history.is_empty() {
            crate::monitor::graphs::paint_history_graph(
                &model.load_history,
                RampId::Util,
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 4,
                    cols: rect.cols.saturating_sub(2),
                    rows: rect.rows.saturating_sub(5),
                },
                surface,
            );
        }
    });
}

fn paint_frequency_history(model: &CpuModel, rect: Rect, surface: &mut Surface) {
    let visible = rect.rows.saturating_sub(3) / 2;
    for (index, policy) in model.cpu_freq.policies.iter().take(visible).enumerate() {
        let y = rect.y + 3 + index * 2;
        if y >= rect.y + rect.rows || rect.cols == 0 {
            break;
        }
        let Some(history) = model
            .frequency_history
            .iter()
            .find(|history| history.policy == policy.policy)
        else {
            continue;
        };
        let values = history.samples.ordered();
        crate::monitor::graphs::paint_sparkline(
            &values,
            rect.cols.saturating_sub(2),
            RampId::Util,
            Rect {
                x: rect.x + 1,
                y,
                cols: rect.cols.saturating_sub(2),
                rows: 1,
            },
            surface,
        );
    }
}

fn paint_cores(grid: &mut CellGrid, rect: Rect, model: &CpuModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Cores (L3)", None);
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    if model.cpu_stat.cores.is_empty() {
        body.put_str(0, 0, mark(model.cpu_stat_health), CellStyle::default());
        return;
    }
    let per_row = if crate::monitor::graphs::table().braille {
        2
    } else {
        1
    };
    let labels = model.cpu_stat.cores.len().div_ceil(per_row).min(area.rows);
    for row in 0..labels {
        let first = row * per_row;
        let first_label = core_label(model, first);
        let second_label = if per_row == 2 {
            model
                .cpu_stat
                .cores
                .get(first + 1)
                .map(|_| core_label(model, first + 1))
        } else {
            None
        };
        let line = match second_label {
            Some(second) => format!("{first_label} / {second}"),
            None => first_label,
        };
        body.put_str(0, row, &line, CellStyle::default());
    }
}

fn core_label(model: &CpuModel, index: usize) -> String {
    let Some(core) = model.cpu_stat.cores.get(index) else {
        return mark(model.cpu_stat_health).to_owned();
    };
    let kind = if matches!(model.topology_health, Health::Absent(_)) {
        ""
    } else {
        model
            .topology
            .cpus
            .iter()
            .find(|cpu| cpu.id == core.id)
            .map(|cpu| match cpu.core_type {
                thegn_metrics::collect::policy::cpu_topology::CoreType::Performance => "P",
                thegn_metrics::collect::policy::cpu_topology::CoreType::Efficiency => "E",
                thegn_metrics::collect::policy::cpu_topology::CoreType::Uniform => "",
            })
            .unwrap_or("")
    };
    let group = if matches!(model.topology_health, Health::Absent(_)) {
        None
    } else {
        model
            .topology
            .l3_groups
            .iter()
            .position(|group| group.contains(&core.id))
    };
    let mut parts = vec![format!("{:>2}", core.id)];
    if !kind.is_empty() {
        parts.push(kind.to_owned());
    }
    if let Some(group) = group {
        parts.push(format!("L3{group}"));
    }
    parts.join(" ")
}

fn paint_core_heatmap(model: &CpuModel, rect: Rect, surface: &mut Surface) {
    if model.core_busy_history.is_empty() {
        return;
    }
    let labels_width = 27.min(rect.cols.saturating_sub(2));
    let graph_rect = Rect {
        x: rect.x + labels_width,
        y: rect.y + 1,
        cols: rect.cols.saturating_sub(labels_width + 1),
        rows: rect.rows.saturating_sub(2),
    };
    if graph_rect.cols == 0 || graph_rect.rows == 0 {
        return;
    }
    let mut rows = [&[][..]; super::CORE_HISTORY_LIMIT];
    let mut ordered_rows = Vec::with_capacity(model.core_busy_history.len());
    let count = model.core_busy_history.len().min(rows.len());
    ordered_rows.extend(
        model
            .core_busy_history
            .iter()
            .take(count)
            .map(|history| history.ordered()),
    );
    for (row, samples) in rows.iter_mut().zip(&ordered_rows) {
        *row = &samples[samples.len().saturating_sub(graph_rect.cols)..];
    }
    let mut out = crate::monitor::graphs::GraphOut::new();
    crate::monitor::graphs::paint_heatmap(
        &mut out,
        &rows[..count],
        RampId::Util,
        &crate::chrome::palette_snapshot(),
    );
    for (row, line) in out.lines.iter().take(graph_rect.rows).enumerate() {
        crate::seg::draw_line(
            surface,
            graph_rect.x,
            graph_rect.y + row,
            graph_rect.cols,
            line,
            Tok::Slot(S::Dim),
        );
    }
}

fn paint_idle(grid: &mut CellGrid, rect: Rect, model: &CpuModel) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "C states residency",
        None,
    );
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    if model.idle.cores.is_empty() {
        body.put_str(0, 0, mark(model.idle_health), CellStyle::default());
        return;
    }
    for (row, core) in model.idle.cores.iter().take(area.rows).enumerate() {
        let c0 = core.c0_pct.map_or_else(
            || mark(model.idle_health).to_owned(),
            |value| format!("{value:.0}%"),
        );
        let states = core
            .states
            .iter()
            .map(|state| {
                let name = state.name.as_deref().unwrap_or("C?");
                let value = state.residency_pct.map_or_else(
                    || mark(model.idle_health).to_owned(),
                    |pct| format!("{pct:.0}%"),
                );
                format!("{name} {value}")
            })
            .collect::<Vec<_>>()
            .join("  ");
        let label = format!("{:>2} C0 {c0}  {states}", core.cpu);
        body.put_str(0, row, &label, CellStyle::default());
        let start_x = label.chars().count().min(area.cols);
        let width = area.cols.saturating_sub(start_x);
        if width == 0 {
            continue;
        }
        let mut cumulative = 0.0_f32;
        let segments = core
            .c0_pct
            .into_iter()
            .chain(core.states.iter().filter_map(|state| state.residency_pct));
        for (index, value) in segments.enumerate() {
            let value = value.max(0.0);
            let from = ((cumulative / 100.0) * width as f32).round() as usize;
            cumulative = (cumulative + value).min(100.0);
            let to = ((cumulative / 100.0) * width as f32).round() as usize;
            let segment_width = to.saturating_sub(from);
            if segment_width == 0 {
                continue;
            }
            let (filled, _) = caps::bar_track(1.0, segment_width);
            body.put_str(
                start_x + from,
                row,
                &filled,
                CellStyle {
                    fg: style(Tok::Ramp(RampId::Util, (index % 8) as u8)),
                    ..CellStyle::default()
                },
            );
        }
    }
}

fn style(tok: Tok) -> StyleTok {
    StyleTok::from_tok(tok).unwrap_or(StyleTok::INHERIT)
}

fn paint_heat_panel(
    rect: Rect,
    title: &str,
    part: &CpuStatPart,
    health: Health,
    total: bool,
    scratch: &mut CpuPaintScratch,
) {
    let CpuPaintScratch {
        grid,
        aux_grid: gauge,
        text: modes,
        ..
    } = scratch;
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), title, None);
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let rows = &part.cores;
    if !total {
        let core_count = rows.len().min(area.cols);
        if core_count == 0 {
            body.put_str(0, 0, mark(health), CellStyle::default());
            return;
        }
        for (index, core) in rows.iter().take(core_count).enumerate() {
            let left = index * area.cols / core_count;
            let right = (index + 1) * area.cols / core_count;
            let busy = core.share.map(|share| share.busy);
            let level = busy.map(|value| value.min(1000));
            let missing = if core.online {
                health
            } else {
                Health::Absent(Absent::NoHardware)
            };
            modes.clear();
            write!(modes, "{}", core.id).expect("writing CPU core id to String");
            if area.rows > 0 {
                body.put_str(
                    left,
                    0,
                    modes.as_str(),
                    CellStyle {
                        fg: style(Tok::Slot(S::Text)),
                        ..CellStyle::default()
                    },
                );
            }
            let Some(level) = level else {
                body.put_str(
                    left,
                    area.rows.saturating_sub(1),
                    mark(missing),
                    CellStyle::default(),
                );
                continue;
            };
            let height = area.rows.saturating_sub(1);
            let scaled = height.saturating_mul(usize::from(level));
            let filled = scaled / 1000;
            let partial = (scaled % 1000).saturating_mul(7) / 1000;
            let color = style(Tok::Heat((usize::from(level) / 200).min(4) as u8));
            for offset in 0..height {
                let y = area.rows - 1 - offset;
                let glyph = if offset < filled {
                    Some(caps::raster_table().hbar8[7])
                } else if offset == filled && partial > 0 {
                    Some(caps::raster_table().hbar8[partial - 1])
                } else {
                    None
                };
                if let Some(glyph) = glyph {
                    for x in left..right {
                        body.put(
                            x,
                            y,
                            CellStyle {
                                ch: glyph,
                                fg: color,
                                ..CellStyle::default()
                            },
                        );
                    }
                }
            }
        }
        return;
    }
    let y = 0;
    let label = "total";
    body.put_str(0, y, label, CellStyle::default());
    if let Some(share) = part.total {
        let value = f32::from(share.busy) / 1000.0;
        modes.clear();
        write!(
            modes,
            "busy {:>2}% u{:>2} s{:>2} io{:>2} irq{:>2} st{:>2}",
            (value * 100.0).round() as u8,
            share.user / 10,
            share.system / 10,
            share.iowait / 10,
            (share.irq + share.softirq) / 10,
            share.steal / 10
        )
        .expect("writing CPU mode labels to String");
        let width = area.cols.saturating_sub(7).saturating_sub(modes.len());
        gauge.resize(width, 1);
        gauge.clear();
        Meter::default().paint(
            gauge,
            Rect::full(width, 1),
            MeterSpec {
                value,
                warn: 0.7,
                crit: 0.9,
                ramp: RampId::Util,
                label: false,
                fmt: NumFmt::default(),
            },
        );
        for x in 0..width {
            if let Some(cell) = gauge.get(x, 0) {
                body.put(7 + x, y, cell);
            }
        }
        body.put_str(
            area.cols.saturating_sub(modes.len()),
            y,
            modes,
            CellStyle::default(),
        );
    } else {
        body.put_str(7.min(area.cols), y, mark(health), CellStyle::default());
    }
}

fn mark(health: Health) -> &'static str {
    match health {
        Health::Absent(Absent::NoPermission) => caps::glyph(Glyph::Lock),
        Health::Absent(_) | Health::Failing { .. } | Health::Ok => caps::active_glyphs().emdash,
        Health::Warming => caps::active_glyphs().ellipsis,
    }
}

fn paint_freq(grid: &mut CellGrid, rect: Rect, part: &CpuFreqPart, health: Health) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Frequency", None);
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let missing = health;
    let profile = part
        .power
        .platform_profile
        .as_ref()
        .map(|token| token.as_str())
        .unwrap_or(mark(missing));
    let profile_line = format!("profile {profile}");
    body.put_str(0, 0, &profile_line, CellStyle::default());
    let count = part.policies.len().min(area.rows.saturating_sub(1) / 2);
    if count == 0 {
        body.put_str(
            0,
            1.min(area.rows.saturating_sub(1)),
            mark(health),
            CellStyle::default(),
        );
        return;
    }
    for (index, policy) in part.policies.iter().take(count).enumerate() {
        let row = index * 2;
        let min = policy.scaling_min_khz.or(policy.hw_min_khz);
        let max = policy.scaling_max_khz.or(policy.hw_max_khz);
        let current = policy.cur_khz;
        let mhz = current
            .map(|khz| format!("{}MHz", khz / 1000))
            .unwrap_or_else(|| mark(missing).to_owned());
        let governor = policy
            .governor
            .as_ref()
            .map(|token| token.as_str())
            .unwrap_or(mark(missing));
        let epp = policy
            .epp
            .as_ref()
            .map(|token| token.as_str())
            .unwrap_or(mark(missing));
        let minimum = min
            .map(|khz| khz / 1000)
            .map_or_else(|| mark(missing).to_owned(), |value| value.to_string());
        let maximum = max
            .map(|khz| khz / 1000)
            .map_or_else(|| mark(missing).to_owned(), |value| value.to_string());
        let label = format!(
            "P{} {mhz} {minimum}-{maximum}MHz {governor}/{epp}",
            policy.policy
        );
        body.put_str(0, row + 1, &label, CellStyle::default());
        if let (Some(min), Some(max), Some(current)) = (min, max, current) {
            let range = max.saturating_sub(min).max(1);
            let value = (current.saturating_sub(min) as f32 / range as f32).clamp(0.0, 1.0);
            let x = label.len().min(area.cols);
            let width = area.cols.saturating_sub(x);
            let (filled, track) = caps::bar_track(value, width);
            body.put_str(
                x,
                row + 1,
                &filled,
                CellStyle {
                    fg: style(Tok::Ramp(RampId::Util, (value * 7.0) as u8)),
                    ..CellStyle::default()
                },
            );
            body.put_str(
                x + filled.chars().count(),
                row + 1,
                &track,
                CellStyle {
                    fg: style(Tok::Slot(S::Dim)),
                    ..CellStyle::default()
                },
            );
        }
    }
}

fn paint_load(grid: &mut CellGrid, rect: Rect, model: &CpuModel, gauge: &mut CellGrid) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "Load & pressure",
        None,
    );
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let load = model.loadavg.value.as_ref();
    let load_line = load
        .map(|v| {
            format!(
                "load {:.2} {:.2} {:.2}",
                f64::from(v.load1) / 100.0,
                f64::from(v.load5) / 100.0,
                f64::from(v.load15) / 100.0
            )
        })
        .unwrap_or_else(|| format!("load {}", mark(model.loadavg_health)));
    body.put_str(0, 0, &load_line, CellStyle::default());
    let running = model
        .cpu_stat
        .procs_running
        .or_else(|| model.loadavg.value.map(|value| value.running))
        .map(|n| n.to_string())
        .unwrap_or_else(|| mark(model.cpu_stat_health).to_owned());
    let runnable = format!("runnable {running}");
    body.put_str(
        0,
        1.min(area.rows.saturating_sub(1)),
        &runnable,
        CellStyle::default(),
    );
    let psi = model
        .psi
        .cpu
        .some_stall_centi
        .map(|v| format!("CPU some {:.2}%", f64::from(v) / 100.0))
        .unwrap_or_else(|| format!("CPU some {}", mark(model.psi_health)));
    let y = 2.min(area.rows.saturating_sub(1));
    body.put_str(0, y, &psi, CellStyle::default());
    if let Some(centi_percent) = model.psi.cpu.some_stall_centi {
        let x = psi.len().min(area.cols);
        let width = area.cols.saturating_sub(x);
        gauge.resize(width, 1);
        gauge.clear();
        Meter::default().paint(
            gauge,
            Rect::full(width, 1),
            MeterSpec {
                value: (centi_percent as f32 / 10_000.0).clamp(0.0, 1.0),
                warn: 0.1,
                crit: 0.3,
                ramp: RampId::Util,
                label: false,
                fmt: NumFmt::default(),
            },
        );
        for x_offset in 0..width {
            if let Some(cell) = gauge.get(x_offset, 0) {
                body.put(x + x_offset, y, cell);
            }
        }
    }
}

mod irq;

fn paint_sched(grid: &mut CellGrid, rect: Rect, part: &CpuSchedPart, health: Health) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "Run queue wait",
        None,
    );
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let visible = area.rows.min(part.cores.len());
    if visible == 0 {
        body.put_str(0, 0, mark(health), CellStyle::default());
        return;
    }
    for (row, core) in part.cores.iter().take(visible).enumerate() {
        let id = format!("CPU{:02}", core.id);
        let missing = if core.online {
            health
        } else {
            Health::Absent(Absent::NoHardware)
        };
        let wait = core
            .run_queue_wait_ms_per_s
            .map(|v| format!("{v:.2} ms/s"))
            .unwrap_or_else(|| mark(missing).to_owned());
        let line = format!("{id} {wait}");
        body.put_str(0, row, &line, CellStyle::default());
        if let Some(wait) = core.run_queue_wait_ms_per_s {
            let max_wait = 100.0_f64;
            let x = line.len().min(area.cols);
            let width = area.cols.saturating_sub(x);
            let (filled, track) = caps::bar_track((wait / max_wait).clamp(0.0, 1.0) as f32, width);
            body.put_str(
                x,
                row,
                &filled,
                CellStyle {
                    fg: style(Tok::Ramp(RampId::Util, 3)),
                    ..CellStyle::default()
                },
            );
            body.put_str(
                x + filled.chars().count(),
                row,
                &track,
                CellStyle {
                    fg: style(Tok::Slot(S::Dim)),
                    ..CellStyle::default()
                },
            );
        }
    }
}

#[cfg(test)]
#[test]
fn consecutive_cpu_paints_reuse_the_style_cache() {
    let model = CpuModel::default();
    let mut surface = Surface::new(80, 20);
    crate::caps::test_override::with_color(thegn_core::termcaps::ColorDepth::Truecolor, || {
        paint(&model, &mut surface, USAGE, Rect::full(80, 20));
        let after_first = PAINT_SCRATCH.with(|scratch| scratch.borrow().emitter.rebuilds());
        paint(&model, &mut surface, USAGE, Rect::full(80, 20));
        let after_second = PAINT_SCRATCH.with(|scratch| scratch.borrow().emitter.rebuilds());
        assert_eq!(after_second, after_first);
    });
}
