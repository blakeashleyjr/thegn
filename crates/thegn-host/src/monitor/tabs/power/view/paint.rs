use super::{EffectiveState, PowerModel};
use crate::caps::{self, Glyph};
use crate::{
    compositor::Rect,
    monitor::kit::{chip, grid::CellGrid},
};
use thegn_core::{
    numfmt::{NumFmt, Reading, Unit},
    theme::StateRole,
};
use thegn_metrics::collect::health::{Absent, Health};

fn style() -> crate::monitor::kit::grid::CellStyle {
    crate::monitor::kit::grid::CellStyle::default()
}
pub(super) fn mark(health: Health) -> &'static str {
    match health {
        Health::Warming => caps::active_glyphs().ellipsis,
        Health::Absent(Absent::NoPermission) => caps::glyph(Glyph::Lock),
        Health::Absent(_) | Health::Failing { .. } | Health::Ok => caps::active_glyphs().emdash,
    }
}
fn put_line(grid: &mut CellGrid, rect: Rect, row: usize, text: &str) {
    grid.view(rect).put_str(0, row, text, style());
}
pub(super) fn paint_battery(grid: &mut CellGrid, rect: Rect, model: &PowerModel) {
    if model.batteries.is_empty() {
        put_line(
            grid,
            rect,
            0,
            &format!("battery {}", mark(model.power_supply_health)),
        );
        return;
    }
    for (index, battery) in model.batteries.iter().take(rect.rows / 2).enumerate() {
        let capacity = battery
            .capacity
            .map(|v| format!("{v}%"))
            .unwrap_or_else(|| mark(model.power_supply_health).to_owned());
        let state = match battery.state {
            Some(EffectiveState::Charging) => "charging",
            Some(EffectiveState::Discharging) => "discharging",
            Some(EffectiveState::Held) => "held",
            Some(EffectiveState::Full) => "full",
            Some(EffectiveState::IdleOnAc) => "idle on AC",
            None => "unknown",
        };
        put_line(
            grid,
            rect,
            index * 2,
            &format!("{}  {capacity}  {state}", battery.name),
        );
        if index * 2 + 1 < rect.rows {
            let energy = battery
                .now_wh
                .zip(battery.full_wh)
                .map(|(a, b)| format!("{a:.1}/{b:.1} Wh"))
                .unwrap_or_else(|| mark(model.power_supply_health).to_owned());
            put_line(
                grid,
                rect,
                index * 2 + 1,
                &format!(
                    "{energy}  health {}  cycles {}",
                    battery
                        .full_wh
                        .zip(battery.design_wh)
                        .filter(|(_, d)| *d > 0.0)
                        .map(|(a, b)| format!("{:.1}%", a / b * 100.0))
                        .unwrap_or_else(|| mark(model.power_supply_health).to_owned()),
                    battery
                        .cycles
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| mark(model.power_supply_health).to_owned())
                ),
            );
        }
    }
    let row = model.batteries.len().saturating_mul(2);
    if row < rect.rows {
        put_line(
            grid,
            rect,
            row,
            &format!(
                "draw direction {}  ETA {}",
                model
                    .battery_w
                    .map(|v| format!("{v:.2}W"))
                    .unwrap_or_else(|| mark(model.power_supply_health).to_owned()),
                model
                    .eta_hours
                    .map(|v| format!("{v:.1}h"))
                    .unwrap_or_else(|| mark(model.power_supply_health).to_owned())
            ),
        );
    }
    if row + 1 < rect.rows {
        for (index, adapter) in model.adapters.iter().take(rect.rows - row - 1).enumerate() {
            put_line(
                grid,
                rect,
                row + 1 + index,
                &format!(
                    "{} {}",
                    adapter.name,
                    adapter
                        .online
                        .map(|v| if v { "online" } else { "offline" })
                        .map(str::to_owned)
                        .unwrap_or_else(|| mark(model.power_supply_health).to_owned())
                ),
            );
        }
    }
    if model.telemetry_battery_history.is_empty() && rect.rows > 0 {
        put_line(
            grid,
            rect,
            rect.rows - 1,
            &format!("charge history {}", mark(model.power_supply_health)),
        );
    }
}
pub(super) fn paint_rails(grid: &mut CellGrid, rect: Rect, model: &PowerModel) {
    let values = [
        ("socket", model.socket_w),
        ("APU", model.apu_w),
        ("core", model.core_w),
        ("gfx", model.gfx_w),
        ("system", model.system_w),
        ("battery", model.battery_w),
    ];
    let mut row = rect.rows.saturating_sub(11);
    if matches!(model.powercap_health, Health::Absent(Absent::NoPermission)) {
        put_line(
            grid,
            rect,
            row,
            &format!("{} RAPL root-only", caps::glyph(Glyph::Lock)),
        );
        row += 1;
    } else if model.powercap_health == Health::Warming {
        put_line(grid, rect, row, caps::active_glyphs().ellipsis);
        row += 1;
    }
    for (label, value) in values {
        if row >= rect.rows {
            break;
        }
        put_line(
            grid,
            rect,
            row,
            &format!(
                "{label:8} {}",
                value.map(|v| format!("{v:.2} W")).unwrap_or_else(|| {
                    mark(
                        if label == "socket"
                            || label == "APU"
                            || label == "core"
                            || label == "gfx"
                            || label == "system"
                        {
                            model.amd_smu_health
                        } else {
                            model.power_supply_health
                        },
                    )
                    .to_owned()
                })
            ),
        );
        row += 1;
    }
    for zone in model
        .zones
        .iter()
        .filter(|zone| zone.active)
        .take(rect.rows.saturating_sub(row))
    {
        put_line(
            grid,
            rect,
            row,
            &format!(
                "{} {}",
                zone.name,
                zone.watts
                    .map(|v| format!("{v:.2} W"))
                    .unwrap_or_else(|| mark(model.powercap_health).to_owned())
            ),
        );
        row += 1;
    }
    if row < rect.rows {
        put_line(
            grid,
            rect,
            row,
            &if model.session_started {
                format!("session {:.3} Wh", model.session.energy_wh())
            } else {
                format!(
                    "session {}",
                    mark(
                        if model.power_supply_health == Health::Warming
                            || model.powercap_health == Health::Warming
                        {
                            Health::Warming
                        } else {
                            model.power_supply_health
                        }
                    )
                )
            },
        );
    }
}
pub(super) fn paint_policy(grid: &mut CellGrid, rect: Rect, model: &PowerModel) {
    let profile = model
        .policy
        .platform_profile
        .map(|v| format!("profile {}", v.as_str()))
        .unwrap_or_else(|| format!("profile {}", mark(model.cpu_freq_health)));
    let boost = model
        .policy
        .boost
        .map(|enabled| format!("boost {}", if enabled { "on" } else { "off" }))
        .unwrap_or_else(|| format!("boost {}", mark(model.cpu_freq_health)));
    let epp = model
        .cpu_policy
        .first()
        .and_then(|policy| policy.epp)
        .map(|token| format!("EPP {}", token.as_str()))
        .unwrap_or_else(|| format!("EPP {}", mark(model.cpu_freq_health)));
    let mut chip_x = rect.x;
    let mut chip_y = rect.y;
    for (label, state) in [
        (
            profile,
            if model.policy.platform_profile.is_some() {
                StateRole::Ok
            } else {
                StateRole::Unknown
            },
        ),
        (
            boost,
            if model.policy.boost.is_some() {
                StateRole::Ok
            } else {
                StateRole::Unknown
            },
        ),
        (
            epp,
            if model
                .cpu_policy
                .first()
                .and_then(|policy| policy.epp)
                .is_some()
            {
                StateRole::Ok
            } else {
                StateRole::Unknown
            },
        ),
    ]
    .into_iter()
    {
        let chip_width = label.len().saturating_add(1).min(rect.cols);
        if chip_x > rect.x && chip_x + chip_width > rect.x + rect.cols {
            chip_x = rect.x;
            chip_y += 1;
        }
        if chip_y >= rect.y + rect.rows {
            break;
        }
        chip::draw(
            grid,
            Rect {
                x: chip_x,
                y: chip_y,
                cols: chip_width.min(rect.x + rect.cols - chip_x),
                rows: 1,
            },
            &label,
            Reading::NotApplicable,
            Unit::Count,
            NumFmt::default(),
            state,
        );
        chip_x += chip_width;
    }
    let chip_rows = chip_y.saturating_sub(rect.y) + 1;
    for (row, policy) in model
        .cpu_policy
        .iter()
        .take(rect.rows.saturating_sub(chip_rows))
        .enumerate()
    {
        put_line(
            grid,
            rect,
            row + chip_rows,
            &format!(
                "policy {}  {} / {} / EPP {}",
                policy.policy,
                policy
                    .driver
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(|| mark(model.cpu_freq_health).to_owned()),
                policy
                    .governor
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(|| mark(model.cpu_freq_health).to_owned()),
                policy
                    .epp
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(|| mark(model.cpu_freq_health).to_owned())
            ),
        );
    }
}
