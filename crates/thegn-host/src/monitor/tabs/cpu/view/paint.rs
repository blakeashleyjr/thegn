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
use termwiz::surface::Surface;
use thegn_core::{numfmt::NumFmt, theme::RampId};
use thegn_metrics::collect::{
    health::Health,
    model::{cpu_freq::CpuFreqPart, cpu_sched::CpuSchedPart, cpu_stat::CpuStatPart},
};

const USAGE: PanelId = PanelId("cpu.usage");
const CORES: PanelId = PanelId("cpu.cores");
const FREQ: PanelId = PanelId("cpu.freq");
const LOAD: PanelId = PanelId("cpu.load");
const IRQ: PanelId = PanelId("cpu.irq");
const SCHED: PanelId = PanelId("cpu.sched");

pub(super) fn paint(model: &CpuModel, surface: &mut Surface, id: PanelId, rect: Rect) {
    let mut grid = CellGrid::new(rect.cols, rect.rows);
    match id {
        USAGE => paint_heat_panel(
            &mut grid,
            rect,
            "CPU usage",
            &model.cpu_stat,
            model.cpu_stat_health,
            true,
        ),
        CORES => paint_heat_panel(
            &mut grid,
            rect,
            "Cores",
            &model.cpu_stat,
            model.cpu_stat_health,
            false,
        ),
        FREQ => paint_freq(&mut grid, rect, &model.cpu_freq, model.cpu_freq_health),
        LOAD => paint_load(&mut grid, rect, model),
        IRQ => irq::paint_irq(&mut grid, rect, &model.cpu_irq, model.cpu_irq_health),
        SCHED => paint_sched(&mut grid, rect, &model.cpu_sched, model.sched_health),
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

fn style(tok: Tok) -> StyleTok {
    StyleTok::from_tok(tok).unwrap_or(StyleTok::INHERIT)
}

fn paint_heat_panel(
    grid: &mut CellGrid,
    rect: Rect,
    title: &str,
    part: &CpuStatPart,
    health: Health,
    total: bool,
) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), title, None);
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let rows = &part.cores;
    if !total {
        let core_count = rows.len().min(area.cols);
        if core_count == 0 {
            body.put_str(
                0,
                0,
                mark(if health == Health::Ok {
                    Health::Ok
                } else {
                    health
                }),
                CellStyle::default(),
            );
            return;
        }
        for (index, core) in rows.iter().take(core_count).enumerate() {
            let left = index * area.cols / core_count;
            let right = (index + 1) * area.cols / core_count;
            let busy = core.share.map(|share| share.busy);
            let level = busy.map_or(0, |value| (value.saturating_mul(7) / 1000) as usize);
            let glyph = caps::raster_table().hbar8[level.min(7)];
            let color = style(Tok::Heat((level / 2).min(4) as u8));
            let id = format!("{}", core.id);
            if area.rows > 0 {
                body.put_str(
                    left,
                    0,
                    &id,
                    CellStyle {
                        fg: style(Tok::Slot(S::Text)),
                        ..CellStyle::default()
                    },
                );
            }
            for y in 1..area.rows {
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
        return;
    }
    let mut y = 0;
    if total {
        let label = "total";
        body.put_str(0, y, label, CellStyle::default());
        let value = part.total.map(|share| f32::from(share.busy) / 1000.0);
        if let Some(value) = value {
            let width = area.cols.saturating_sub(26);
            let mut gauge = CellGrid::new(width, 1);
            Meter::default().paint(
                &mut gauge,
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
            let modes = format!(
                "{:>3}% io{:>2}% st{:>2}%",
                (value * 100.0).round() as u8,
                part.total.map_or(0, |share| share.iowait / 10),
                part.total.map_or(0, |share| share.steal / 10)
            );
            body.put_str(
                area.cols.saturating_sub(modes.len()),
                y,
                &modes,
                CellStyle::default(),
            );
        } else {
            body.put_str(
                7.min(area.cols),
                y,
                mark(if health == Health::Ok {
                    Health::Ok
                } else {
                    health
                }),
                CellStyle::default(),
            );
        }
        y += 1;
    }
    if rows.is_empty() {
        body.put_str(
            0,
            y,
            mark(if health == Health::Ok {
                Health::Ok
            } else {
                health
            }),
            CellStyle::default(),
        );
        return;
    }
    let visible = area.rows.saturating_sub(y).min(rows.len());
    for (line, core) in rows.iter().take(visible).enumerate() {
        let y = y + line;
        let label = format!("CPU{:02}", core.id);
        body.put_str(
            0,
            y,
            &label,
            CellStyle {
                fg: style(Tok::Slot(S::Ghost)),
                ..CellStyle::default()
            },
        );
        let start = 6.min(area.cols);
        let suffix = core.share.map_or_else(
            || mark(Health::Warming).to_owned(),
            |share| {
                format!(
                    "{:>3}% io{:>2}% st{:>2}%",
                    share.busy / 10,
                    share.iowait / 10,
                    share.steal / 10
                )
            },
        );
        let width = area.cols.saturating_sub(start).saturating_sub(suffix.len());
        if let Some(share) = core.share {
            let fraction = f32::from(share.busy) / 1000.0;
            let (filled, track) = caps::bar_track(fraction, width);
            body.put_str(
                start,
                y,
                &filled,
                CellStyle {
                    fg: style(Tok::Ramp(RampId::Util, (fraction * 7.0) as u8)),
                    ..CellStyle::default()
                },
            );
            body.put_str(
                start + filled.chars().count(),
                y,
                &track,
                CellStyle {
                    fg: style(Tok::Slot(S::Dim)),
                    ..CellStyle::default()
                },
            );
            body.put_str(
                area.cols.saturating_sub(suffix.len()),
                y,
                &suffix,
                CellStyle::default(),
            );
        } else {
            body.put_str(start, y, mark(Health::Warming), CellStyle::default());
        }
    }
}

fn mark(health: Health) -> &'static str {
    match health {
        Health::Absent(_) => caps::glyph(Glyph::Lock),
        Health::Warming | Health::Failing { .. } => caps::active_glyphs().ellipsis,
        Health::Ok => caps::active_glyphs().emdash,
    }
}

fn paint_freq(grid: &mut CellGrid, rect: Rect, part: &CpuFreqPart, health: Health) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Frequency", None);
    let area = body.rect();
    if area.cols == 0 || area.rows == 0 {
        return;
    }
    let missing = if health == Health::Ok {
        Health::Ok
    } else {
        health
    };
    let profile = part
        .power
        .platform_profile
        .map(|token| token.as_str())
        .unwrap_or(mark(missing));
    let profile_line = format!("profile {profile}");
    body.put_str(0, 0, &profile_line, CellStyle::default());
    let count = part.policies.len().min(area.rows.saturating_sub(1));
    if count == 0 {
        body.put_str(
            0,
            1.min(area.rows.saturating_sub(1)),
            mark(if health == Health::Ok {
                Health::Ok
            } else {
                health
            }),
            CellStyle::default(),
        );
        return;
    }
    for (row, policy) in part.policies.iter().take(count).enumerate() {
        let min = policy.scaling_min_khz.or(policy.hw_min_khz);
        let max = policy.scaling_max_khz.or(policy.hw_max_khz);
        let current = policy.cur_khz;
        let mhz = current
            .map(|khz| format!("{}MHz", khz / 1000))
            .unwrap_or_else(|| mark(missing).to_owned());
        let governor = policy
            .governor
            .map(|token| token.as_str())
            .unwrap_or(mark(missing));
        let epp = policy
            .epp
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

fn paint_load(grid: &mut CellGrid, rect: Rect, model: &CpuModel) {
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
        let mut gauge = CellGrid::new(width, 1);
        Meter::default().paint(
            &mut gauge,
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
        let wait = core
            .run_queue_wait_ms_per_s
            .map(|v| format!("{v:.2} ms/s"))
            .unwrap_or_else(|| mark(health).to_owned());
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
