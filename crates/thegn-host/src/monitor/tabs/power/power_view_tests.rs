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
fn tail_from_column(line: &str, column: usize) -> String {
    line.chars().skip(column).collect()
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
        let rail_periods = [5_usize, 7, 9, 11];
        let rail_bases = [9_000_u32, 2_000, 3_000, 20_000];
        let metrics = &mut frame.parts.amd_smu.cards[0].metrics;
        metrics.power_apu = Some(rail_bases[0] + (index % rail_periods[0]) as u32 * 250);
        metrics.power_gfx = Some(rail_bases[1] + (index % rail_periods[1]) as u32 * 250);
        metrics.power_all_core = Some(rail_bases[2] + (index % rail_periods[2]) as u32 * 250);
        metrics.power_sys = Some(rail_bases[3] + (index % rail_periods[3]) as u32 * 250);
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
                    let rails_rect = placed
                        .iter()
                        .find(|panel| panel.id.0 == "power.rails")
                        .expect("rails panel is placed")
                        .rect;
                    board.paint(&view, &mut surface, area, 0, &[]);
                    let output = text(&mut surface);
                    assert!(output.contains("Power rails"));
                    if cols < 240 {
                        let graph_left = rails_rect.x + 1;
                        let graph_right = rails_rect.x + rails_rect.cols - 2;
                        let socket_row = output
                            .lines()
                            .position(|line| {
                                tail_from_column(line, graph_left).starts_with("socket ")
                            })
                            .expect("socket graph label is drawn");
                        assert!(
                            surface.screen_lines()[socket_row + 1]
                                .visible_cells()
                                .any(|cell| {
                                    cell.cell_index() == graph_right
                                        && !cell.str().is_empty()
                                        && cell.str() != " "
                                        && cell.str() != "\u{2800}"
                                }),
                            "compact socket band needs a plotted row"
                        );
                    }
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
                        // density follow-up (batch 52): estimated, primary sets from the printed value
                        if unicode == UnicodeLevel::Ascii {
                            assert!(r >= 0.24, "ASCII fill ratio {r:.3}, floor 0.24");
                        } else {
                            assert!(r >= 0.19, "Unicode fill ratio {r:.3}, floor 0.19");
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
fn policy_chips_wrap_without_clipping_the_epp_label() {
    let mut model = PowerModel::default();
    model.absorb_frame(&fixture());
    let mut grid = crate::monitor::kit::grid::CellGrid::new(40, 3);
    super::paint::paint_policy(&mut grid, Rect::full(40, 3), &model);
    let line = |row| {
        (0..40)
            .map(|col| grid.get(col, row).map_or(' ', |cell| cell.ch))
            .collect::<String>()
    };
    assert!(line(0).contains("profile balanced"));
    assert!(line(0).contains("boost on"));
    assert!(line(1).contains("EPP balance_perf"));
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
    assert_eq!(model.rail_history[0].last(), Some(&45.0));
    assert_eq!(super::rail_ceiling(&model), 30.0);
    let view = PowerView(&model);
    let mut surface = Surface::new(240, 60);
    Board::default().paint(&view, &mut surface, Rect::full(240, 60), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains("30 W STAPM; peak 45 W"));
}

#[test]
fn powercap_zone_histories_follow_identity_when_source_order_changes() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    for (index, zone) in frame.parts.powercap.zones.iter_mut().enumerate() {
        zone.watts = Some((index + 1) as f32);
    }
    model.absorb_frame(&frame);
    let original_core_len = model
        .zones
        .iter()
        .find(|row| row.domain == Some(PowerDomain::Core))
        .expect("core zone exists")
        .history
        .len();
    frame.at_ms += 1_000;
    frame.parts.powercap.zones.swap(1, 3);
    frame
        .parts
        .powercap
        .zones
        .retain(|zone| zone.domain != PowerDomain::Core);
    for zone in &mut frame.parts.powercap.zones {
        zone.watts = Some(match zone.domain {
            PowerDomain::Package => 11.0,
            PowerDomain::Core => 22.0,
            PowerDomain::Uncore => 33.0,
            PowerDomain::Dram => 44.0,
            PowerDomain::Psys | PowerDomain::Other => 55.0,
        });
    }
    model.absorb_frame(&frame);
    for (domain, expected) in [
        (PowerDomain::Package, 11.0),
        (PowerDomain::Uncore, 33.0),
        (PowerDomain::Dram, 44.0),
    ] {
        let row = model
            .zones
            .iter()
            .find(|row| row.domain == Some(domain))
            .expect("zone identity retained");
        assert!(row.active);
        assert_eq!(row.history.last(), Some(&expected));
    }
    let dormant_core = model
        .zones
        .iter()
        .find(|row| row.domain == Some(PowerDomain::Core))
        .expect("dormant core slot retained");
    assert!(!dormant_core.active);
    assert_eq!(dormant_core.history.len(), original_core_len);
    frame.at_ms += 1_000;
    frame.parts.powercap.zones.push(PowercapZone {
        name: Arc::from("core"),
        domain: PowerDomain::Core,
        watts: Some(22.0),
        energy_uj: Some(20_000_000),
    });
    frame.parts.powercap.zones.reverse();
    model.absorb_frame(&frame);
    let restored_core = model
        .zones
        .iter()
        .find(|row| row.domain == Some(PowerDomain::Core))
        .expect("core identity restored");
    assert!(restored_core.active);
    assert_eq!(restored_core.history.len(), original_core_len + 1);
    assert_eq!(restored_core.history.last(), Some(&22.0));
}

#[test]
fn full_zone_slots_match_present_identities_before_reusing_a_dormant_slot() {
    let mut frame = fixture();
    frame.parts.powercap.zones.clear();
    for index in 0..super::INSTANCE_CAP {
        frame.parts.powercap.zones.push(PowercapZone {
            name: Arc::from(format!("zone-{index}")),
            domain: PowerDomain::Package,
            watts: Some(index as f32 + 1.0),
            energy_uj: None,
        });
    }
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);

    frame.at_ms += 1_000;
    frame.parts.powercap.zones.remove(7);
    for (index, zone) in frame.parts.powercap.zones.iter_mut().enumerate() {
        zone.watts = Some(100.0 + index as f32);
    }
    model.absorb_frame(&frame);
    let expected_present = frame
        .parts
        .powercap
        .zones
        .iter()
        .map(|zone| (zone.name.to_string(), zone.watts.expect("watts present")))
        .collect::<Vec<_>>();

    frame.at_ms += 1_000;
    frame.parts.powercap.zones.insert(
        0,
        PowercapZone {
            name: Arc::from("new-zone"),
            domain: PowerDomain::Package,
            watts: Some(65.0),
            energy_uj: None,
        },
    );
    for (index, zone) in frame.parts.powercap.zones.iter_mut().skip(1).enumerate() {
        zone.watts = Some(200.0 + index as f32);
    }
    model.absorb_frame(&frame);

    for (index, zone) in frame.parts.powercap.zones.iter().skip(1).enumerate() {
        let row = model
            .zones
            .iter()
            .find(|row| row.name == zone.name.as_ref())
            .expect("present zone retains its identity");
        assert!(row.active);
        assert_eq!(row.history.last(), Some(&(200.0 + index as f32)));
        let previous = expected_present
            .iter()
            .find(|(name, _)| name == zone.name.as_ref())
            .map(|(_, watts)| *watts)
            .expect("previous identity recorded");
        assert_eq!(row.history.get(row.history.len() - 2), Some(&previous));
    }
    let new_zone = model
        .zones
        .iter()
        .find(|row| row.name == "new-zone")
        .expect("new zone occupies the dormant slot");
    assert_eq!(new_zone.history, [65.0]);
}

#[test]
fn power_rail_scale_includes_powercap_only_draw_and_hides_stale_smu_rings() {
    let mut frame = fixture();
    frame.parts.amd_smu.cards.clear();
    frame.parts.powercap.zones = vec![PowercapZone {
        name: Arc::from("package-65w"),
        domain: PowerDomain::Package,
        watts: Some(65.0),
        energy_uj: None,
    }];
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    let ceiling = super::rail_ceiling(&model);
    assert!(ceiling >= 70.0, "powercap-only rail ceiling {ceiling}");
    assert!(
        model.zones[0]
            .history
            .iter()
            .all(|watts| watts / ceiling <= 1.0)
    );
    let output = {
        let mut surface = Surface::new(240, 60);
        Board::default().paint(
            &PowerView(&model),
            &mut surface,
            Rect::full(240, 60),
            0,
            &[],
        );
        text(&mut surface)
    };
    assert!(output.contains("Power rails (70 W max)"));

    let mut amd_frame = fixture();
    model = PowerModel::default();
    model.absorb_frame(&amd_frame);
    amd_frame.at_ms += 1_000;
    amd_frame.parts.amd_smu.cards.clear();
    model.absorb_frame(&amd_frame);
    assert!(model.rail_history.iter().all(Vec::is_empty));
    let mut surface = Surface::new(240, 60);
    Board::default().paint(
        &PowerView(&model),
        &mut surface,
        Rect::full(240, 60),
        0,
        &[],
    );
    assert!(!text(&mut surface).contains("socket "));
}

#[test]
fn warmed_power_and_telemetry_rings_keep_capacity_over_one_thousand_absorbs() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    let mut telemetry = TelemetryHistory::default();
    for tick in 0..8_u64 {
        telemetry.push(
            &thegn_metrics::StatsSnapshot {
                battery: Some((40 + tick as u8, false)),
                battery_power_w: Some(2.0 + tick as f32),
                ..thegn_metrics::StatsSnapshot::default()
            },
            tick * 1_000,
        );
    }
    for tick in 0..1_000_u64 {
        frame.at_ms = tick * 1_000;
        frame.parts.amd_smu.cards[0].metrics.power_socket = Some(12_000 + tick as u32 % 9_000);
        for (index, zone) in frame.parts.powercap.zones.iter_mut().enumerate() {
            zone.watts = Some(index as f32 + (tick % 7) as f32 * 0.1);
        }
        model.absorb_frame(&frame);
        model.absorb_history(
            &telemetry,
            crate::telemetry::Window::default(),
            crate::telemetry::ScaleMode::Window,
            7_000,
        );
    }
    let capacities = (
        model.rail_history[0].capacity(),
        model.rail_history.each_ref().map(Vec::capacity),
        model
            .zones
            .iter()
            .map(|row| row.history.capacity())
            .collect::<Vec<_>>(),
        model.telemetry_battery_history.capacity(),
        model.telemetry_battery_power_history.capacity(),
    );
    let series_storage = model.series_storage();
    let (_, allocations) = crate::proc_workload_alloc::measure(|| {
        for tick in 1_000..2_000_u64 {
            frame.at_ms = tick * 1_000;
            frame.parts.amd_smu.cards[0].metrics.power_socket = Some(12_000 + tick as u32 % 9_000);
            model.absorb_frame(&frame);
            model.absorb_history(
                &telemetry,
                crate::telemetry::Window::default(),
                crate::telemetry::ScaleMode::Window,
                7_000,
            );
        }
    });
    assert_eq!(
        (allocations.allocation_calls, allocations.realloc_calls),
        (0, 0),
        "warmed Power and SeriesBuf absorbs allocated: {allocations:?}"
    );
    assert_eq!(
        (
            model.rail_history[0].capacity(),
            model.rail_history.each_ref().map(Vec::capacity),
            model
                .zones
                .iter()
                .map(|row| row.history.capacity())
                .collect::<Vec<_>>(),
            model.telemetry_battery_history.capacity(),
            model.telemetry_battery_power_history.capacity(),
        ),
        capacities,
        "warmed Power ring capacity changed after 1,000 absorbs"
    );
    assert_eq!(
        model.series_storage(),
        series_storage,
        "SeriesBuf storage moved after warm-up"
    );
}

#[test]
fn absent_battery_keeps_power_rails_and_policy_visible() {
    let mut frame = fixture();
    frame
        .parts
        .power_supply
        .supplies
        .retain(|supply| supply.kind != SupplyKind::Battery);
    frame.health[CollectorId::PowerSupply.index()] = Health::Absent(Absent::NoHardware);
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    let view = PowerView(&model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains("battery"));
    assert!(output.contains(caps::active_glyphs().emdash));
    assert!(output.contains("Power rails"));
    assert!(output.contains("socket"));
    assert!(output.contains("Power policy"));
    assert!(output.contains("profile balanced"));
}

#[test]
fn newest_powercap_and_amd_rail_samples_reach_the_last_graph_column() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    for tick in 0..HISTORY_SEED as u32 {
        frame.at_ms += 1_000;
        frame.parts.amd_smu.cards[0].metrics.power_socket = Some(10_000 + tick * 1_000);
        frame.parts.amd_smu.cards[0].metrics.power_apu = Some(8_000 + tick * 500);
        frame.parts.amd_smu.cards[0].metrics.power_all_core = Some(5_000 + tick * 400);
        frame.parts.amd_smu.cards[0].metrics.power_gfx = Some(1_000 + tick * 200);
        frame.parts.amd_smu.cards[0].metrics.power_sys = Some(20_000 + tick * 1_000);
        for (index, zone) in frame.parts.powercap.zones.iter_mut().enumerate() {
            zone.watts = Some(2.0 + index as f32 + tick as f32 * 0.4);
        }
        model.absorb_frame(&frame);
    }
    let view = PowerView(&model);
    let area = Rect::full(240, 60);
    let board = Board::default();
    let rails = board
        .placed(&view, area)
        .into_iter()
        .find(|panel| panel.id.0 == "power.rails")
        .expect("rails panel is placed");
    let graph_right = rails.rect.x + rails.rect.cols - 2;
    let graph_left = rails.rect.x + 1;
    let graph_top = rails.rect.y + 1;
    let mut surface = Surface::new(240, 60);
    test_override::with_unicode(UnicodeLevel::Full, || {
        board.paint(&view, &mut surface, area, 0, &[])
    });
    let output = text(&mut surface);
    let output_lines = output.lines().collect::<Vec<_>>();
    let lines = surface.screen_lines();
    for label in [
        "socket",
        "APU",
        "core",
        "gfx",
        "system",
        "package-0",
        "uncore",
        "dram",
    ] {
        let matching_rows = output_lines
            .iter()
            .enumerate()
            .skip(graph_top)
            .filter(|(_, line)| {
                tail_from_column(line, graph_left).starts_with(&format!("{label} "))
            })
            .map(|(row, _)| row)
            .collect::<Vec<_>>();
        assert!(
            !matching_rows.is_empty(),
            "missing power graph label {label}"
        );
        for label_row in matching_rows {
            assert!(
                (label_row + 1..label_row + 3).any(|sample_row| {
                    lines[sample_row].visible_cells().any(|cell| {
                        cell.cell_index() == graph_right
                            && !cell.str().is_empty()
                            && cell.str() != " "
                            && cell.str() != "\u{2800}"
                    })
                }),
                "newest {label} sample should occupy last inner graph column"
            );
        }
    }
}

#[test]
fn socket_ring_does_not_switch_to_package_power() {
    let mut frame = MonitorFrame::default();
    frame.parts.powercap.zones.push(PowercapZone {
        name: Arc::from("package-0"),
        domain: PowerDomain::Package,
        watts: Some(18.0),
        energy_uj: Some(20_000_000),
    });
    frame.parts.powercap.zones.push(PowercapZone {
        name: Arc::from("core"),
        domain: PowerDomain::Core,
        watts: Some(8.0),
        energy_uj: Some(10_000_000),
    });
    let mut model = PowerModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.socket_w, Some(18.0));
    assert!(model.rail_history[0].is_empty());
    assert_eq!(model.zones[0].history, [18.0]);
    assert_eq!(model.zones[1].history, [8.0]);
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
    model.absorb_frame(&fixture());
    model.absorb_history(
        &history,
        crate::telemetry::Window::default(),
        crate::telemetry::ScaleMode::Window,
        121_000,
    );
    assert!(!model.telemetry_battery_history.is_empty());
    assert_eq!(model.telemetry_battery_history.len(), 240);
    assert_eq!(model.batteries[0].charge_history.len(), 1);
    assert!(
        model
            .telemetry_battery_history
            .last()
            .is_some_and(|value| (*value - 0.63).abs() < 0.001),
        "latest 63% charge stays at 63% of the labelled 100% scale"
    );
    assert!(model.telemetry_battery_history[0].is_nan());
    assert!(
        model.telemetry_battery_history[237..]
            .iter()
            .all(|value| (*value - 0.63).abs() < 0.001)
    );
    assert!(
        model.telemetry_battery_power_history[237..]
            .iter()
            .all(|value| value.is_finite())
    );
    assert_eq!(model.telemetry_draw_peak_w, Some(14.0));
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
    assert!(output.contains("battery draw peak 14.0 W"));
    assert!(output.contains("draw direction -14.00W"));
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
fn telemetry_history_paints_finite_runs_without_turning_nan_into_a_bar() {
    let values = [0.8, 0.7, f32::NAN, 0.6, 0.9];
    let mut surface = Surface::new(values.len(), 4);
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        super::paint_history_runs(&values, Rect::full(values.len(), 4), &mut surface);
    });
    let has_ink = |column| {
        surface.screen_lines().iter().any(|line| {
            line.visible_cells().any(|cell| {
                cell.cell_index() == column && !cell.str().is_empty() && cell.str() != " "
            })
        })
    };
    assert!(has_ink(0), "finite run before NaN is painted");
    assert!(
        !has_ink(2),
        "NaN gap remains blank instead of becoming a minimum bar"
    );
    assert!(has_ink(4), "finite run after NaN is painted");
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
    assert_eq!(model.rail_history[0].len(), super::HISTORY_CAP);
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
