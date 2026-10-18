use super::{EffectiveState, PowerModel};
use crate::{compositor::Rect, monitor::kit::grid::CellGrid};

fn style() -> crate::monitor::kit::grid::CellStyle {
    crate::monitor::kit::grid::CellStyle::default()
}
fn absent() -> String {
    crate::caps::active_glyphs().emdash.to_owned()
}
fn put_line(grid: &mut CellGrid, rect: Rect, row: usize, text: &str) {
    grid.view(rect).put_str(0, row, text, style());
}
pub(super) fn paint_battery(grid: &mut CellGrid, rect: Rect, model: &PowerModel) {
    if model.batteries.is_empty() {
        put_line(grid, rect, 0, &format!("battery {}", absent()));
        return;
    }
    for (index, battery) in model.batteries.iter().take(rect.rows / 2).enumerate() {
        let capacity = battery
            .capacity
            .map(|v| format!("{v}%"))
            .unwrap_or_else(absent);
        let state = match model.state {
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
                .unwrap_or_else(absent);
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
                        .unwrap_or_else(absent),
                    battery.cycles.map(|v| v.to_string()).unwrap_or_else(absent)
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
                "signed power {}  ETA {}",
                model
                    .battery_w
                    .map(|v| format!("{v:.2}W"))
                    .unwrap_or_else(absent),
                model
                    .eta_hours
                    .map(|v| format!("{v:.1}h"))
                    .unwrap_or_else(absent)
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
                        .unwrap_or_else(absent)
                ),
            );
        }
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
                value.map(|v| format!("{v:.2} W")).unwrap_or_else(absent)
            ),
        );
        row += 1;
    }
    for zone in model.zones.iter().take(rect.rows.saturating_sub(row)) {
        put_line(
            grid,
            rect,
            row,
            &format!(
                "{} {}",
                zone.name,
                zone.watts
                    .map(|v| format!("{v:.2} W"))
                    .unwrap_or_else(absent)
            ),
        );
        row += 1;
    }
    if row < rect.rows {
        put_line(
            grid,
            rect,
            row,
            &format!("session {:.3} Wh", model.session.energy_wh()),
        );
    }
}
pub(super) fn paint_policy(grid: &mut CellGrid, rect: Rect, model: &PowerModel) {
    let profile = model
        .policy
        .platform_profile
        .map(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(absent);
    put_line(
        grid,
        rect,
        0,
        &format!(
            "profile {profile}  boost {}  AMD pstate {}",
            model
                .policy
                .boost
                .map(|v| if v { "on" } else { "off" })
                .unwrap_or_else(absent),
            model
                .policy
                .amd_pstate_status
                .map(|v| v.as_str().to_owned())
                .unwrap_or_else(absent)
        ),
    );
    for (row, policy) in model
        .cpu_policy
        .iter()
        .take(rect.rows.saturating_sub(1))
        .enumerate()
    {
        put_line(
            grid,
            rect,
            row + 1,
            &format!(
                "policy {}  {} | {} | EPP {}",
                policy.policy,
                policy
                    .driver
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(absent),
                policy
                    .governor
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(absent),
                policy
                    .epp
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(absent)
            ),
        );
    }
}
