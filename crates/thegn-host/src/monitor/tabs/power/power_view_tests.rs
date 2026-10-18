use super::{PowerModel, PowerView};
use crate::telemetry::TelemetryHistory;
use crate::{
    caps::{self, test_override},
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        layout::PanelId,
    },
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::{
        amd_smu::{AmdSmuCard, AmdSmuStatus},
        cpu_policy::{CpuPolicy, SysToken},
        power_supply::{SupplyKind, SupplyReading, SupplyStatus},
        powercap::{PowerDomain, PowercapZone},
    },
    parse::amd_gpu_metrics::{Decoded, GpuMetrics},
};

fn fixture() -> MonitorFrame {
    let mut frame = MonitorFrame {
        at_ms: 1_000,
        ..MonitorFrame::default()
    };
    frame.parts.power_supply.supplies.push(SupplyReading {
        name: Arc::from("BAT1"),
        kind: SupplyKind::Battery,
        device_scope: false,
        online: None,
        status: Some(SupplyStatus::Discharging),
        capacity_pct: Some(73),
        energy_now_uwh: Some(39_000_000),
        energy_full_uwh: Some(53_600_000),
        energy_full_design_uwh: Some(60_600_000),
        voltage_now_uv: Some(17_470_000),
        voltage_min_design_uv: Some(15_480_000),
        current_now_ua: Some(-800_000),
        power_now_uw: Some(14_000_000),
        cycle_count: Some(21),
        usb_type_active: None,
    });
    frame.parts.power_supply.supplies.push(SupplyReading {
        name: Arc::from("ACAD"),
        kind: SupplyKind::Mains,
        device_scope: false,
        online: Some(false),
        status: None,
        capacity_pct: None,
        energy_now_uwh: None,
        energy_full_uwh: None,
        energy_full_design_uwh: None,
        voltage_now_uv: None,
        voltage_min_design_uv: None,
        current_now_ua: None,
        power_now_uw: None,
        cycle_count: None,
        usb_type_active: None,
    });
    frame.parts.powercap.zones = [
        ("package-0", PowerDomain::Package, 18.0),
        ("core", PowerDomain::Core, 8.0),
        ("uncore", PowerDomain::Uncore, 2.0),
        ("dram", PowerDomain::Dram, 3.0),
    ]
    .into_iter()
    .map(|(name, domain, watts)| PowercapZone {
        name: Arc::from(name),
        domain,
        watts: Some(watts),
        energy_uj: Some(20_000_000),
    })
    .collect();
    let token = SysToken::new(b"balanced").expect("fixture profile");
    frame.parts.cpu_freq.power.platform_profile = Some(token);
    frame.parts.cpu_freq.power.boost = Some(true);
    frame.parts.cpu_freq.policies.push(CpuPolicy {
        policy: 0,
        driver: Some(SysToken::new(b"amd-pstate-epp").expect("fixture driver")),
        governor: Some(SysToken::new(b"powersave").expect("fixture governor")),
        epp: Some(SysToken::new(b"balance_perf").expect("fixture EPP")),
        ..CpuPolicy::default()
    });
    frame.parts.amd_smu.cards.push(AmdSmuCard {
        card: 0,
        suspended: false,
        decoded: AmdSmuStatus::Decoded(Decoded::Full),
        metrics: GpuMetrics {
            power_socket: Some(17_400),
            power_apu: Some(17_400),
            power_gfx: Some(2_800),
            power_all_core: Some(9_100),
            power_sys: Some(25_000),
            ..GpuMetrics::default()
        },
    });
    frame
}
fn text(surface: &mut Surface) -> String {
    surface.screen_chars_to_string()
}
fn collector_set(ids: &[CollectorId]) -> CollectorSet {
    let mut set = CollectorSet::empty();
    for id in ids {
        set.insert(*id);
    }
    set
}
fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let total = area.cols.min(cols).saturating_mul(area.rows.min(rows));
    if total == 0 {
        return 0.0;
    }
    let occupied = surface
        .screen_lines()
        .iter()
        .take(area.rows)
        .flat_map(|line| line.visible_cells())
        .filter(|cell| {
            cell.cell_index() < area.cols
                && !cell.str().is_empty()
                && cell.str() != " "
                && cell.str() != "\u{2800}"
        })
        .count();
    occupied as f32 / total as f32
}

#[test]
fn absorb_frame_derives_battery_sign_rails_and_profile_from_fixture() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    let mut second_battery = frame.parts.power_supply.supplies[0].clone();
    second_battery.name = Arc::from("BAT2");
    second_battery.status = Some(SupplyStatus::Charging);
    frame.parts.power_supply.supplies.push(second_battery);
    model.absorb_frame(&frame);
    assert_eq!(model.batteries[0].capacity, Some(73));
    assert_eq!(
        model.batteries[0].state,
        Some(super::EffectiveState::Discharging)
    );
    assert_eq!(
        model.batteries[1].state,
        Some(super::EffectiveState::Charging)
    );
    assert_eq!(model.battery_w, Some(-14.0));
    assert_eq!(model.socket_w, Some(17.4));
    assert_eq!(
        model.cpu_policy[0].epp.as_ref().map(SysToken::as_str),
        Some("balance_perf")
    );
    frame.at_ms += 1_000;
    frame.parts.power_supply.supplies[0].status = Some(SupplyStatus::Charging);
    model.absorb_frame(&frame);
    assert_eq!(model.battery_w, Some(14.0));
    frame.parts.amd_smu.cards[0].metrics.power_socket = Some(u32::MAX);
    model.absorb_frame(&frame);
    assert_eq!(model.socket_w, Some(18.0));
    assert!((super::metric_watts(Some(65_535)).unwrap() - 65.535).abs() < 0.001);
}

#[test]
fn snapshot_sizes_capabilities_demand_and_absent_marks() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    for index in 0..HISTORY_SEED {
        frame.at_ms = index as u64 * 1_000;
        frame.parts.amd_smu.cards[0].metrics.power_socket = Some(if index == HISTORY_SEED - 1 {
            17_400
        } else {
            17_400 + ((index % 8) as u32) * 250
        });
        for (zone_index, zone) in frame.parts.powercap.zones.iter_mut().enumerate() {
            let periods = [7_usize, 11, 13, 17];
            let bases = [18.0_f32, 8.0, 2.0, 3.0];
            zone.watts = Some(bases[zone_index] + (index % periods[zone_index]) as f32 * 0.35);
        }
        let battery = &mut frame.parts.power_supply.supplies[0];
        battery.capacity_pct = Some(35 + (index % 55) as u8);
        battery.energy_now_uwh = Some(20_000_000 + (index as u64 % 17) * 1_000_000);
        model.absorb_frame(&frame);
    }
    let mut telemetry = TelemetryHistory::default();
    for tick in 0..HISTORY_SEED as u64 {
        let stats = thegn_metrics::StatsSnapshot {
            battery: Some((35 + (tick % 55) as u8, false)),
            battery_power_w: Some(2.0 + (tick % 13) as f32),
            ..thegn_metrics::StatsSnapshot::default()
        };
        telemetry.push(&stats, tick * 1_000);
    }
    model.absorb_history(
        &telemetry,
        crate::telemetry::Window::default(),
        crate::telemetry::ScaleMode::Window,
        (HISTORY_SEED as u64 - 1) * 1_000,
    );
    assert_eq!(model.batteries[0].charge_history.len(), HISTORY_SEED);
    assert_eq!(model.telemetry_battery_history.len(), 240);
    assert_eq!(model.telemetry_draw_peak_w, Some(14.0));
    assert!(
        model
            .telemetry_battery_power_history
            .iter()
            .all(|value| !value.is_finite() || (0.0..=1.0).contains(value))
    );
    let view = PowerView(&model);
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (depth, unicode) in [
            (ColorDepth::Truecolor, UnicodeLevel::Full),
            (ColorDepth::Ansi16, UnicodeLevel::Full),
            (ColorDepth::Truecolor, UnicodeLevel::Ascii),
        ] {
            test_override::with_color(depth, || {
                test_override::with_unicode(unicode, || {
                    let area = Rect::full(cols, rows);
                    let mut surface = Surface::new(cols, rows);
                    let board = Board::default();
                    let placed = board.placed(&view, area);
                    board.paint(&view, &mut surface, area, 0, &[]);
                    let output = text(&mut surface);
                    assert!(output.contains("Power rails"));
                    assert!(output.contains("BAT1"));
                    assert!(output.contains("17.40 W"));
                    assert!(output.contains("battery draw peak"));
                    assert!(output.contains("balanced"));
                    assert!(output.contains("boost on"));
                    assert!(output.contains("EPP balance_perf"));
                    let mut expected = CollectorSet::empty();
                    for panel in placed {
                        for id in view.demand(panel.id).iter() {
                            expected.insert(id);
                        }
                    }
                    assert_eq!(board.demand(&view, area, 1000).collectors, expected);
                    if cols == 240 {
                        let r = fill_ratio(&surface, area);
                        // density follow-up (batch 50): estimated, primary sets from the printed value
                        if unicode == UnicodeLevel::Ascii {
                            assert!(r >= 0.30, "ASCII fill ratio {r:.3}, floor 0.30");
                        } else {
                            assert!(r >= 0.30, "Unicode fill ratio {r:.3}, floor 0.30");
                        }
                    }
                })
            });
        }
    }
    let empty_model = PowerModel {
        power_supply_health: Health::Absent(Absent::NoHardware),
        powercap_health: Health::Absent(Absent::NoHardware),
        amd_smu_health: Health::Absent(Absent::NoHardware),
        cpu_freq_health: Health::Absent(Absent::NoHardware),
        ..PowerModel::default()
    };
    let empty = PowerView(&empty_model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&empty, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains(caps::active_glyphs().emdash));
    assert!(!output.contains("0.00 W"));
    assert!(!output.contains("session 0.000"));
}

#[test]
fn history_graph_right_aligns_short_series_and_stapm_overage_is_labeled() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    for index in 0..8 {
        frame.at_ms += 1_000;
        frame.parts.amd_smu.cards[0].metrics.power_socket = Some((index + 1) * 5_000);
        model.absorb_frame(&frame);
    }
    let view = PowerView(&model);
    test_override::with_unicode(UnicodeLevel::Full, || {
        let area = Rect::full(240, 60);
        let board = Board::default();
        let rails = board
            .placed(&view, area)
            .into_iter()
            .find(|placed| placed.id.0 == "power.rails")
            .expect("rails panel is placed");
        let graph_right = rails.rect.x + rails.rect.cols - 2;
        let graph_top = rails.rect.y + 1;
        let mut surface = Surface::new(240, 60);
        board.paint(&view, &mut surface, area, 0, &[]);
        assert!(
            surface.screen_lines()[graph_top..graph_top + rails.rect.rows - 12]
                .iter()
                .any(|line| line.visible_cells().any(|cell| {
                    cell.cell_index() == graph_right
                        && !cell.str().is_empty()
                        && cell.str() != " "
                        && cell.str() != "\u{2800}"
                })),
            "right-most history graph column should show the last sample"
        );
    });

    frame.at_ms += 1_000;
    frame.parts.amd_smu.cards[0].metrics.power_socket = Some(45_000);
    frame.parts.amd_smu.cards[0]
        .metrics
        .power_current_stapm_limit = Some(30);
    model.absorb_frame(&frame);
    assert_eq!(model.history.last(), Some(&45.0));
    assert_eq!(super::rail_ceiling(&model), 30.0);
    let view = PowerView(&model);
    let mut surface = Surface::new(240, 60);
    Board::default().paint(&view, &mut surface, Rect::full(240, 60), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains("30 W STAPM; peak 45 W"));
}

#[test]
fn battery_history_uses_latest_telemetry_and_empty_history_is_absent() {
    let mut history = TelemetryHistory::default();
    for tick in 0..24_u64 {
        let stats = thegn_metrics::StatsSnapshot {
            battery: Some((40 + (tick % 50) as u8, false)),
            battery_power_w: Some(2.0 + (tick % 13) as f32),
            ..thegn_metrics::StatsSnapshot::default()
        };
        history.push(&stats, 97_000 + tick * 1_000);
    }
    let mut model = PowerModel::default();
    model.absorb_history(
        &history,
        crate::telemetry::Window::default(),
        crate::telemetry::ScaleMode::Window,
        120_000,
    );
    assert!(!model.telemetry_battery_history.is_empty());
    assert_eq!(model.batteries[0].charge_history.len(), 1);
    assert!(
        model
            .telemetry_battery_history
            .last()
            .is_some_and(|value| *value > 0.0)
    );
    let mut surface = Surface::new(240, 60);
    Board::default().paint(
        &PowerView(&model),
        &mut surface,
        Rect::full(240, 60),
        0,
        &[],
    );
    let view = PowerView(&model);
    let battery = Board::default()
        .placed(&view, Rect::full(240, 60))
        .into_iter()
        .find(|panel| panel.id.0 == "power.battery")
        .expect("battery panel is placed");
    let inner_rows = battery.rect.rows.saturating_sub(2);
    let graph_rows = (inner_rows.saturating_sub(5) / 2).min(6);
    let graph_y = battery.rect.y + 1 + inner_rows - graph_rows * 2;
    let charge_end = graph_y + graph_rows;
    let power_start = charge_end + 1;
    let power_end = graph_y + graph_rows * 2;
    let inner_cols = battery.rect.cols.saturating_sub(2);
    let telemetry_cols = inner_cols * 2 / 3;
    let power_right = battery.rect.x + 1 + telemetry_cols - 1;
    let has_graph_ink = |x| {
        surface.screen_lines()[graph_y + 1..charge_end]
            .iter()
            .any(|line| {
                line.visible_cells().any(|cell| {
                    cell.cell_index() == x
                        && !cell.str().is_empty()
                        && cell.str() != " "
                        && cell.str() != "\u{2800}"
                })
            })
    };
    let right = battery.rect.x + battery.rect.cols - 2;
    assert!(
        has_graph_ink(right),
        "latest charge sample reaches the graph's right edge"
    );
    assert!(
        !has_graph_ink(battery.rect.x + 1),
        "short history leaves the panel's left graph edge blank"
    );
    assert!(
        surface.screen_lines()[power_start..power_end]
            .iter()
            .any(|line| {
                line.visible_cells().any(|cell| {
                    cell.cell_index() == power_right
                        && !cell.str().is_empty()
                        && cell.str() != " "
                        && cell.str() != "\u{2800}"
                })
            }),
        "newest draw sample reaches the last telemetry graph column"
    );
    let output = text(&mut surface);
    let absent_line = format!("charge history {}", caps::active_glyphs().emdash);
    assert!(!output.contains(&absent_line));
    let mut no_history = PowerModel::default();
    no_history.absorb_frame(&fixture());
    no_history.power_supply_health = Health::Absent(Absent::NoHardware);
    let mut absent_surface = Surface::new(100, 30);
    Board::default().paint(
        &PowerView(&no_history),
        &mut absent_surface,
        Rect::full(100, 30),
        0,
        &[],
    );
    let absent_output = text(&mut absent_surface);
    assert!(absent_output.contains(&format!("charge history {}", caps::active_glyphs().emdash)));
}

#[test]
fn half_full_battery_energy_graph_uses_capacity_scale() {
    let mut frame = fixture();
    frame.parts.power_supply.supplies[0].energy_now_uwh = Some(26_800_000);
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    let view = PowerView(&model);
    let area = Rect::full(240, 60);
    let battery = Board::default()
        .placed(&view, area)
        .into_iter()
        .find(|panel| panel.id.0 == "power.battery")
        .expect("battery panel is placed");
    let mut surface = Surface::new(240, 60);
    Board::default().paint(&view, &mut surface, area, 0, &[]);
    let inner_rows = battery.rect.rows.saturating_sub(2);
    let graph_rows = (inner_rows.saturating_sub(5) / 2).min(6);
    let graph_y = battery.rect.y + 1 + inner_rows - graph_rows * 2;
    let row_height = graph_rows * 2;
    let energy_y = graph_y + row_height / 2;
    let energy_rows = row_height - row_height / 2;
    let energy_x = battery.rect.x + 1 + (battery.rect.cols - 2) * 2 / 3;
    let energy_end = battery.rect.x + battery.rect.cols - 1;
    let occupied_rows = surface.screen_lines()[energy_y..energy_y + energy_rows]
        .iter()
        .filter(|line| {
            line.visible_cells().any(|cell| {
                cell.cell_index() >= energy_x
                    && cell.cell_index() < energy_end
                    && !cell.str().is_empty()
                    && cell.str() != " "
                    && cell.str() != "\u{2800}"
            })
        })
        .count();
    assert!(
        (2..=4).contains(&occupied_rows),
        "half-full energy should occupy about half the graph rows, got {occupied_rows}"
    );
}

#[test]
fn rail_histories_follow_identity_when_powercap_zones_reorder() {
    let mut frame = fixture();
    let mut model = PowerModel::default();
    for tick in 0..4_u32 {
        frame.parts.powercap.zones[0].watts = Some(10.0 + tick as f32);
        model.absorb_frame(&frame);
    }
    let package_before = model
        .zones
        .iter()
        .find(|zone| zone.name == "package-0")
        .expect("package rail exists")
        .history
        .clone();
    frame.parts.powercap.zones.reverse();
    frame.parts.powercap.zones[3].watts = Some(14.0);
    model.absorb_frame(&frame);
    let package_after = model
        .zones
        .iter()
        .find(|zone| zone.name == "package-0")
        .expect("package rail remains after reorder");
    assert_eq!(package_after.history.len(), package_before.len() + 1);
    assert_eq!(package_after.history.last(), Some(&14.0));
    let core_after = model
        .zones
        .iter()
        .find(|zone| zone.name == "core")
        .expect("core rail remains after reorder");
    assert_eq!(core_after.history.last(), Some(&8.0));
    let capacities: Vec<usize> = model
        .zones
        .iter()
        .map(|zone| zone.history.capacity())
        .collect();
    for tick in 0..1_000_u32 {
        frame.at_ms += 1_000;
        model.absorb_frame(&frame);
        frame.parts.powercap.zones[3].watts = Some(14.0 + (tick % 4) as f32);
    }
    assert!(
        model
            .zones
            .iter()
            .map(|zone| zone.history.capacity())
            .eq(capacities)
    );
}

#[test]
fn demand_is_static_for_each_panel_and_history_reuses_capacity_after_warmup() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    let mut telemetry = TelemetryHistory::default();
    for tick in 0..8_u64 {
        let stats = thegn_metrics::StatsSnapshot {
            battery: Some((50 + (tick % 20) as u8, false)),
            battery_power_w: Some(3.0 + tick as f32),
            ..thegn_metrics::StatsSnapshot::default()
        };
        telemetry.push(&stats, tick * 1_000);
    }
    model.absorb_history(
        &telemetry,
        crate::telemetry::Window::default(),
        crate::telemetry::ScaleMode::Window,
        7_000,
    );
    let capacities = (
        model.batteries.capacity(),
        model.adapters.capacity(),
        model.zones.capacity(),
        model.cpu_policy.capacity(),
        model.history_capacity(),
        model.telemetry_battery_history.capacity(),
        model.telemetry_battery_power_history.capacity(),
    );
    for index in 0..1_000 {
        frame.at_ms += 1_000 + index as u64;
        model.absorb_frame(&frame);
        model.absorb_history(
            &telemetry,
            crate::telemetry::Window::default(),
            crate::telemetry::ScaleMode::Window,
            7_000,
        );
    }
    assert_eq!(
        (
            model.batteries.capacity(),
            model.adapters.capacity(),
            model.zones.capacity(),
            model.cpu_policy.capacity(),
            model.history_capacity(),
            model.telemetry_battery_history.capacity(),
            model.telemetry_battery_power_history.capacity(),
        ),
        capacities
    );
    assert_eq!(model.history.len(), super::HISTORY_CAP);
    assert!(model.batteries.iter().all(|battery| {
        battery.charge_history.capacity() == super::HISTORY_CAP
            && battery.energy_history.capacity() == super::HISTORY_CAP
    }));
    let view = PowerView(&model);
    let battery = collector_set(&[CollectorId::PowerSupply]);
    let rails = collector_set(&[
        CollectorId::PowerSupply,
        CollectorId::Powercap,
        CollectorId::AmdSmu,
    ]);
    let policy = collector_set(&[CollectorId::CpuFreq]);
    assert_eq!(view.demand(PanelId("power.battery")), battery);
    assert_eq!(view.demand(PanelId("power.rails")), rails);
    assert_eq!(view.demand(PanelId("power.policy")), policy);
    let union = collector_set(&[
        CollectorId::PowerSupply,
        CollectorId::Powercap,
        CollectorId::AmdSmu,
        CollectorId::CpuFreq,
    ]);
    assert_eq!(
        union,
        [battery, rails, policy]
            .into_iter()
            .fold(CollectorSet::empty(), |mut all, set| {
                for id in set.iter() {
                    all.insert(id);
                }
                all
            })
    );
    assert_eq!(
        Board::default()
            .demand(&view, Rect::full(100, 30), 1000)
            .collectors,
        union
    );
}

#[test]
fn power_health_is_copied_and_renders_warming_and_permission_denied_marks() {
    let mut frame = fixture();
    frame.health[CollectorId::PowerSupply.index()] = Health::Warming;
    frame.health[CollectorId::Powercap.index()] = Health::Absent(Absent::NoPermission);
    frame.health[CollectorId::AmdSmu.index()] = Health::Absent(Absent::NoHardware);
    frame.health[CollectorId::CpuFreq.index()] = Health::Warming;
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.power_supply_health, Health::Warming);
    assert_eq!(model.powercap_health, Health::Absent(Absent::NoPermission));
    assert_eq!(model.amd_smu_health, Health::Absent(Absent::NoHardware));
    assert_eq!(model.cpu_freq_health, Health::Warming);
    model.zones.clear();
    model.socket_w = None;
    let view = PowerView(&model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains("RAPL root-only"));
    assert!(output.contains(caps::active_glyphs().ellipsis));
    assert!(!output.contains("session 0.000"));
}

#[test]
fn empty_warming_frame_renders_ellipsis_for_unavailable_values() {
    let frame = MonitorFrame::default();
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    let view = PowerView(&model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    assert!(text(&mut surface).contains(caps::active_glyphs().ellipsis));
}
const HISTORY_SEED: usize = 120;
