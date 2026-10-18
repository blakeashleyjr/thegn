use super::{PowerModel, PowerView};
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
        .filter(|cell| cell.cell_index() < area.cols && !cell.str().is_empty() && cell.str() != " ")
        .count();
    occupied as f32 / total as f32
}

#[test]
fn absorb_frame_derives_battery_sign_rails_and_profile_from_fixture() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    assert_eq!(model.batteries[0].capacity, Some(73));
    assert_eq!(model.battery_w, Some(-14.0));
    assert_eq!(model.socket_w, Some(17.4));
    assert_eq!(
        model.cpu_policy[0].epp.map(|token| token.as_str()),
        Some("balance_perf")
    );
    frame.at_ms += 1_000;
    frame.parts.power_supply.supplies[0].status = Some(SupplyStatus::Charging);
    model.absorb_frame(&frame);
    assert_eq!(model.battery_w, Some(14.0));
    frame.parts.amd_smu.cards[0].metrics.power_socket = Some(u32::MAX);
    model.absorb_frame(&frame);
    assert_eq!(model.socket_w, Some(18.0));
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
        model.absorb_frame(&frame);
    }
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
                    assert!(output.contains("balanced"));
                    let mut expected = CollectorSet::empty();
                    for panel in placed {
                        for id in view.demand(panel.id).iter() {
                            expected.insert(id);
                        }
                    }
                    assert_eq!(board.demand(&view, area, 1000).collectors, expected);
                    if cols == 240 && unicode == UnicodeLevel::Ascii {
                        // ASCII columns are a one-cell-per-sample line; follow-up: a capability-aware dense history treatment.
                        assert!(
                            fill_ratio(&surface, area) >= 0.08,
                            "ASCII fill {:.3}",
                            fill_ratio(&surface, area)
                        );
                    } else if cols == 240 {
                        // Honest fill from sampled rails/history and dossier; follow-up: richer adapter and process details when their frame fields land.
                        assert!(
                            fill_ratio(&surface, area) >= 0.30,
                            "fill {:.3}",
                            fill_ratio(&surface, area)
                        );
                    }
                })
            });
        }
    }
    let empty_model = PowerModel::default();
    let empty = PowerView(&empty_model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&empty, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains(caps::active_glyphs().emdash));
    assert!(!output.contains("0.00 W"));
}

#[test]
fn demand_is_static_for_each_panel_and_history_reuses_capacity_after_warmup() {
    let mut model = PowerModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    let capacities = (
        model.batteries.capacity(),
        model.adapters.capacity(),
        model.zones.capacity(),
        model.cpu_policy.capacity(),
        model.history_capacity(),
    );
    for index in 0..1_000 {
        frame.at_ms += 1_000 + index as u64;
        model.absorb_frame(&frame);
    }
    assert_eq!(
        (
            model.batteries.capacity(),
            model.adapters.capacity(),
            model.zones.capacity(),
            model.cpu_policy.capacity(),
            model.history_capacity(),
        ),
        capacities
    );
    assert_eq!(model.history.len(), super::HISTORY_CAP);
    let view = PowerView(&model);
    assert!(
        view.demand(PanelId("power.battery"))
            .contains(CollectorId::PowerSupply)
    );
    assert!(
        view.demand(PanelId("power.rails"))
            .contains(CollectorId::Powercap)
    );
    assert!(
        view.demand(PanelId("power.policy"))
            .contains(CollectorId::CpuFreq)
    );
    let empty_model = PowerModel::default();
    let empty_view = PowerView(&empty_model);
    for panel in ["power.battery", "power.rails", "power.policy"] {
        assert_eq!(
            view.demand(PanelId(panel)),
            empty_view.demand(PanelId(panel))
        );
    }
}
const HISTORY_SEED: usize = 120;
