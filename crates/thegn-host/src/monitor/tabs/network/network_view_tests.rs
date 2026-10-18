use super::view::{HEALTH, IFACE_CAP, INTERFACES, NetworkModel, NetworkView, THROUGHPUT, WIFI};
use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
use crate::{
    caps,
    compositor::Rect,
    monitor::board::{Board, TabView},
};
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::Health,
    instances::InstanceId,
    model::net_wifi::{InlineBytes, WifiRow},
    model::netdev::{IfaceSample, ScopeTotal},
    policy::net_scope::{IfaceKind, Scope},
};

fn iface(id: u16, name: &[u8], scope: Scope, rx: f64, tx: f64) -> IfaceSample {
    IfaceSample {
        id: InstanceId(id),
        name: thegn_metrics::collect::model::netdev::IfaceName::new(name),
        kind: match scope {
            Scope::Physical => IfaceKind::Ethernet,
            Scope::Vpn => IfaceKind::Wireguard,
            _ => IfaceKind::Unknown,
        },
        scope,
        rx_bps: Some(rx),
        tx_bps: Some(tx),
        rx_pps: Some(rx / 100.0),
        tx_pps: Some(tx / 100.0),
        rx_errs_ps: Some(f64::from(id % 3)),
        tx_errs_ps: Some(0.0),
        rx_drop_ps: Some(0.0),
        tx_drop_ps: Some(0.0),
        ..IfaceSample::default()
    }
}

fn frame(tick: u16, order: &[u16]) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::Netdev.index()] = Health::Ok;
    frame.health[CollectorId::NetWifi.index()] = Health::Ok;
    let mut ssid = InlineBytes::default();
    ssid.set(format!("test-net-{tick}").as_bytes());
    frame.parts.net_wifi.ifaces.push(WifiRow {
        ifindex: 7,
        name: {
            let mut name = InlineBytes::default();
            name.set(b"wlan0");
            name
        },
        ssid: Some(ssid),
        bssid_masked: Some("aa:bb:xx:ee:ff"),
        freq_mhz: Some(5180 + u32::from(tick % 3) * 20),
        width_mhz: Some(80),
        signal_dbm: Some(-54 + i8::try_from(tick % 4).expect("small signal delta")),
        signal_avg_dbm: Some(-55),
        tx_retries: Some(u32::from(tick)),
        tx_failed: Some(u32::from(tick / 2)),
        connected_secs: Some(u32::from(tick) * 60),
        ..WifiRow::default()
    });
    frame.parts.netdev.ifaces = order
        .iter()
        .map(|id| {
            let scope = if *id == 9 {
                Scope::Vpn
            } else {
                Scope::Physical
            };
            iface(
                *id,
                format!("if{id}").as_bytes(),
                scope,
                f64::from(tick) * 17.0 + f64::from(*id),
                f64::from(tick) * 9.0 + f64::from(*id),
            )
        })
        .collect();
    let physical_rx = frame
        .parts
        .netdev
        .ifaces
        .iter()
        .filter(|item| item.scope == Scope::Physical)
        .filter_map(|item| item.rx_bps)
        .sum();
    let physical_tx = frame
        .parts
        .netdev
        .ifaces
        .iter()
        .filter(|item| item.scope == Scope::Physical)
        .filter_map(|item| item.tx_bps)
        .sum();
    frame.parts.netdev.totals[Scope::Physical.index()] = ScopeTotal {
        rx_bps: Some(physical_rx),
        tx_bps: Some(physical_tx),
    };
    frame.parts.netdev.all = ScopeTotal {
        rx_bps: Some(
            frame
                .parts
                .netdev
                .ifaces
                .iter()
                .filter_map(|item| item.rx_bps)
                .sum(),
        ),
        tx_bps: Some(
            frame
                .parts
                .netdev
                .ifaces
                .iter()
                .filter_map(|item| item.tx_bps)
                .sum(),
        ),
    };
    frame
}

fn telemetry_history() -> TelemetryHistory {
    let mut history = TelemetryHistory::default();
    for tick in 0..120_u64 {
        history.push(
            &thegn_metrics::StatsSnapshot {
                net_bps: Some((20_000 + tick * 1_700, 8_000 + tick * 630)),
                ..thegn_metrics::StatsSnapshot::default()
            },
            1_000 + tick * 1_000,
        );
    }
    history
}

fn lines(surface: &Surface) -> Vec<String> {
    surface
        .screen_lines()
        .iter()
        .map(|line| {
            line.visible_cells()
                .map(|cell| cell.str().to_owned())
                .collect()
        })
        .collect()
}

fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let x0 = area.x.min(cols);
    let y0 = area.y.min(rows);
    let x1 = area.x.saturating_add(area.cols).min(cols);
    let y1 = area.y.saturating_add(area.rows).min(rows);
    let cells = x1.saturating_sub(x0).saturating_mul(y1.saturating_sub(y0));
    if cells == 0 {
        return 0.0;
    }
    let occupied: usize = surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(y, _)| *y >= y0 && *y < y1)
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| {
            let x = cell.cell_index();
            x >= x0
                && x < x1
                && !cell.str().is_empty()
                && cell.str() != " "
                && cell.str() != "\u{2800}"
        })
        .map(|cell| cell.width())
        .sum();
    occupied as f32 / cells as f32
}

#[test]
fn mirrored_sources_use_physical_or_all_and_interface_history_follows_ids() {
    let mut model = NetworkModel::default();
    let first = frame(1, &[1, 2, 9]);
    model.absorb_frame(&first);
    assert_eq!(model.total, first.parts.netdev.physical_or_all());
    assert_ne!(model.total, first.parts.netdev.all);
    let retained = model
        .interfaces
        .iter()
        .find(|entry| entry.id == InstanceId(1))
        .expect("interface inserted")
        .rx
        .len;
    model.absorb_frame(&frame(2, &[2, 1, 9]));
    let after = model
        .interfaces
        .iter()
        .find(|entry| entry.id == InstanceId(1))
        .expect("interface retained after reorder");
    assert_eq!(after.rx.len, retained + 1);
    assert_eq!(after.sample.name.as_str(), "if1");
    let mut values = [0.0; 2];
    let len = after.rx.copy_ordered(&mut values);
    assert_eq!(&values[..len], &[18.0, 35.0]);
}

#[test]
fn reused_interface_slot_starts_with_only_the_new_interfaces_sample() {
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame(1, &[0, 1, 2, 3, 4, 5, 6, 7]));
    model.absorb_frame(&frame(2, &[1, 2, 3, 4, 5, 6, 7, 8]));
    let replacement = model
        .interfaces
        .iter()
        .find(|entry| entry.id == InstanceId(8))
        .expect("replacement interface occupies the dropped slot");
    assert_eq!(replacement.rx.len, 1);
    let mut values = [0.0; 1];
    assert_eq!(replacement.rx.copy_ordered(&mut values), 1);
    assert_eq!(values, [42.0]);
}

#[test]
fn interface_slots_are_capped_and_absorb_capacity_stays_warm() {
    let mut model = NetworkModel::default();
    let ids: Vec<u16> = (0..12).collect();
    model.absorb_frame(&frame(1, &ids));
    assert_eq!(model.interfaces.len(), IFACE_CAP);
    assert_eq!(model.overflow, 4);
    let capacities = (
        model.interfaces.capacity(),
        model.error_history.values.capacity(),
        model.drop_history.values.capacity(),
        model
            .interfaces
            .iter()
            .map(|item| (item.rx.values.capacity(), item.tx.values.capacity()))
            .collect::<Vec<_>>(),
    );
    for tick in 2..1002_u16 {
        model.absorb_frame(&frame(tick, &ids));
    }
    assert_eq!(model.interfaces.capacity(), capacities.0);
    assert_eq!(model.error_history.values.capacity(), capacities.1);
    assert_eq!(model.drop_history.values.capacity(), capacities.2);
    assert_eq!(
        model
            .interfaces
            .iter()
            .map(|item| (item.rx.values.capacity(), item.tx.values.capacity()))
            .collect::<Vec<_>>(),
        capacities.3
    );
}

#[test]
fn wifi_panel_shows_ssid_masked_bssid_and_signal_without_raw_mac() {
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame(1, &[1]));
    let view = NetworkView(&model);
    let placed = Board::default()
        .placed(&view, Rect::full(240, 60))
        .into_iter()
        .find(|panel| panel.id == WIFI)
        .expect("NetWifi health is available, so Wi-Fi panel is placed")
        .rect;
    let mut surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        caps::test_override::with_color(ColorDepth::Truecolor, || {
            Board::default().paint(&view, &mut surface, Rect::full(240, 60), 0, &[]);
        })
    });
    let rendered = lines(&surface).join("\n");
    assert!(rendered.contains("test-net-1"), "SSID missing: {rendered}");
    assert!(
        rendered.contains("aa:bb:xx:ee:ff"),
        "masked BSSID missing: {rendered}"
    );
    assert!(rendered.contains("-53 dBm"), "signal missing: {rendered}");
    assert!(
        rendered.contains("avg -55"),
        "average signal missing: {rendered}"
    );
    assert!(rendered.contains("5 GHz (5200 MHz)"));
    assert!(rendered.contains("80 MHz"));
    assert!(
        rendered.contains("1 / 0"),
        "retry/failure counts missing: {rendered}"
    );
    assert!(
        rendered.contains("1m"),
        "connected time missing: {rendered}"
    );
    assert!(!rendered.contains("aa:bb:cc:dd:ee:ff"));
    assert!(placed.rows > 0);
    let newest = surface.screen_lines()[placed.y + 2]
        .visible_cells()
        .find(|cell| cell.cell_index() == placed.x + placed.cols - 2)
        .expect("Wi-Fi sparkline reaches its last inner column");
    assert_ne!(
        newest.str(),
        " ",
        "newest signal sample is not right-aligned"
    );
    assert_ne!(newest.str(), "\u{2800}", "newest signal sample is blank");
    assert!(view.demand(WIFI).contains(CollectorId::NetWifi));

    let mut no_wifi = frame(2, &[1]);
    no_wifi.health[CollectorId::NetWifi.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NotImplemented);
    model.absorb_frame(&no_wifi);
    let absent_view = NetworkView(&model);
    assert!(
        !Board::default()
            .placed(&absent_view, Rect::full(240, 60))
            .iter()
            .any(|panel| panel.id == WIFI)
    );
    assert!(absent_view.demand(WIFI).is_empty());

    let mut missing_signal = frame(3, &[1]);
    missing_signal.parts.net_wifi.ifaces[0].signal_dbm = None;
    let mut missing_model = NetworkModel::default();
    missing_model.absorb_frame(&missing_signal);
    let mut missing_surface = Surface::new(240, 60);
    let missing_view = NetworkView(&missing_model);
    Board::default().paint(
        &missing_view,
        &mut missing_surface,
        Rect::full(240, 60),
        0,
        &[],
    );
    let missing_rendered = lines(&missing_surface).join("\n");
    assert!(missing_rendered.contains(&format!("{} avg -55", caps::active_glyphs().emdash)));
    assert!(!missing_rendered.contains("0 dBm"));
    assert!(missing_model.wifi[0].signal.values[0].is_nan());
}

#[test]
fn wifi_signal_ring_stays_warm_after_one_thousand_absorbs() {
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame(1, &[1]));
    let ring_capacity = model.wifi[0].signal.values.capacity();
    for tick in 2..1002_u16 {
        model.absorb_frame(&frame(tick, &[1]));
    }
    assert_eq!(model.wifi[0].signal.values.capacity(), ring_capacity);
    assert_eq!(model.wifi[0].signal.len, 120);
    let view = NetworkView(&model);
    let wifi = Board::default()
        .placed(&view, Rect::full(240, 60))
        .into_iter()
        .find(|panel| panel.id == WIFI)
        .expect("Wi-Fi panel is placed")
        .rect;
    let mut surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        Board::default().paint(&view, &mut surface, Rect::full(240, 60), 0, &[]);
    });
    let newest = surface.screen_lines()[wifi.y + 2]
        .visible_cells()
        .find(|cell| cell.cell_index() == wifi.x + wifi.cols - 2)
        .expect("newest Wi-Fi sample occupies the last inner column");
    assert_ne!(newest.str(), " ");
    assert_ne!(newest.str(), "\u{2800}");
}

#[test]
fn cloned_warming_rings_keep_their_logical_capacity() {
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame(1, &[1]));
    let mut cloned = model.clone();
    for tick in 2..40_u16 {
        cloned.absorb_frame(&frame(tick, &[1]));
    }
    let history = cloned
        .interfaces
        .iter()
        .find(|entry| entry.id == InstanceId(1))
        .expect("interface history remains present");
    assert_eq!(history.rx.len, 32);
}

#[test]
fn warming_interfaces_show_ellipsis_and_demands_are_static_netdev() {
    let mut frame = MonitorFrame::default();
    let mut warming_iface = iface(1, b"eth0", Scope::Physical, 0.0, 0.0);
    warming_iface.rx_bps = None;
    warming_iface.tx_bps = None;
    frame.parts.netdev.ifaces.push(warming_iface);
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.health, Health::Warming);
    let view = NetworkView(&model);
    for panel in [THROUGHPUT, INTERFACES, HEALTH] {
        let mut expected = CollectorSet::empty();
        expected.insert(CollectorId::Netdev);
        assert_eq!(view.demand(panel), expected);
    }
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let throughput = Board::default()
        .placed(&view, Rect::full(100, 30))
        .into_iter()
        .find(|panel| panel.id == THROUGHPUT)
        .expect("throughput panel is placed")
        .rect;
    let warming_lines = lines(&surface);
    assert!(warming_lines[throughput.y + 2].contains(caps::active_glyphs().ellipsis));
    assert!(
        warming_lines
            .join("\n")
            .contains(caps::active_glyphs().ellipsis)
    );
    let mut absent = frame;
    absent.health[CollectorId::Netdev.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NoPermission);
    model.absorb_frame(&absent);
    let absent_view = NetworkView(&model);
    for panel in [THROUGHPUT, INTERFACES, HEALTH] {
        assert!(absent_view.demand(panel).is_empty());
    }
    let mut absent_surface = Surface::new(100, 30);
    Board::default().paint(
        &absent_view,
        &mut absent_surface,
        Rect::full(100, 30),
        0,
        &[],
    );
    let absent_rendered = lines(&absent_surface).join("\n");
    assert!(
        absent_rendered.contains("root-only"),
        "root-only chip missing: {absent_rendered}"
    );
    assert!(absent_rendered.contains(caps::glyph(caps::Glyph::Lock)));
    assert!(
        absent_rendered
            .lines()
            .any(|line| line.contains("eth0") && line.contains("root-only"))
    );
    assert!(!absent_rendered.contains("… B/s"));
    assert!(!absent_rendered.contains("…"));
}

#[test]
fn short_throughput_history_is_right_aligned_and_scaled() {
    let mut model = NetworkModel::default();
    model.absorb_frame(&frame(1, &[1]));
    model.absorb_frame(&frame(2, &[1]));
    let mut history = TelemetryHistory::default();
    for tick in 0..3_u64 {
        history.push(
            &thegn_metrics::StatsSnapshot {
                net_bps: Some((10_000 + tick * 2_000, 5_000 + tick * 1_000)),
                ..thegn_metrics::StatsSnapshot::default()
            },
            1_000 + tick * 1_000,
        );
    }
    model.absorb_history(&history, Window::from_secs(10), ScaleMode::Window, 4_000);
    let view = NetworkView(&model);
    let area = Rect::full(100, 30);
    let throughput = Board::default()
        .placed(&view, area)
        .into_iter()
        .find(|panel| panel.id == THROUGHPUT)
        .expect("throughput panel is placed")
        .rect;
    let mut surface = Surface::new(100, 30);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        caps::test_override::with_color(ColorDepth::Truecolor, || {
            Board::default().paint(&view, &mut surface, area, 0, &[]);
        })
    });
    let (cols, _) = surface.dimensions();
    let right_x = (throughput.x + throughput.cols - 2).min(cols - 1);
    let graph_y = throughput.y + 3;
    let screen_lines = surface.screen_lines();
    let cell = screen_lines[graph_y]
        .visible_cells()
        .find(|cell| cell.cell_index() == right_x)
        .expect("rightmost graph cell exists");
    assert_ne!(
        cell.str(),
        "\u{2800}",
        "trailing telemetry gap must draw the held throughput value"
    );
    let mut rx = [0.0; super::view::TELEMETRY_HISTORY];
    let len = model.history_rx.copy_ordered(&mut rx);
    assert!(len > 3);
    assert_eq!(rx[len - 1], rx[len - 2]);
    let rendered = lines(&surface).join("\n");
    assert!(rendered.contains("window"));
    assert!(lines(&surface)[throughput.y + throughput.rows - 2].contains("tx"));
}

#[test]
fn interior_telemetry_gap_keeps_its_bucket_position() {
    let mut history = TelemetryHistory::default();
    history.push(
        &thegn_metrics::StatsSnapshot {
            net_bps: Some((10_000, 5_000)),
            ..thegn_metrics::StatsSnapshot::default()
        },
        1_000,
    );
    history.push(
        &thegn_metrics::StatsSnapshot {
            net_bps: None,
            ..thegn_metrics::StatsSnapshot::default()
        },
        2_000,
    );
    history.push(
        &thegn_metrics::StatsSnapshot {
            net_bps: Some((30_000, 15_000)),
            ..thegn_metrics::StatsSnapshot::default()
        },
        3_000,
    );
    let mut model = NetworkModel::default();
    model.absorb_history(&history, Window::from_secs(4), ScaleMode::Window, 4_000);
    let mut rx = [0.0; super::view::TELEMETRY_HISTORY];
    let len = model.history_rx.copy_ordered(&mut rx);
    assert_eq!(
        len, 360,
        "retained ring must preserve its span after leading gaps"
    );
    assert!(rx[120..240].iter().all(|value| *value == 1.0 / 3.0));
    assert!(
        rx[240..len].iter().any(|value| value.is_finite()),
        "later sample shifted away"
    );
    assert!(
        rx[240..len]
            .iter()
            .all(|value| (*value - 1.0).abs() <= f32::EPSILON)
    );
}

#[test]
fn snapshots_cover_view_sizes_capabilities_and_static_demand() {
    let sizes = [(100, 30), (160, 45), (240, 60)];
    let levels = [
        (UnicodeLevel::Full, ColorDepth::Truecolor),
        (UnicodeLevel::Full, ColorDepth::Ansi16),
        (UnicodeLevel::Ascii, ColorDepth::None),
    ];
    let ids: Vec<u16> = (0..8).collect();
    let mut model = NetworkModel::default();
    for tick in 1..121_u16 {
        model.absorb_frame(&frame(tick, &ids));
    }
    model.absorb_history(
        &telemetry_history(),
        Window::from_secs(120),
        ScaleMode::Window,
        121_000,
    );
    let view = NetworkView(&model);
    for (cols, rows) in sizes {
        for (unicode, depth) in levels {
            let mut surface = Surface::new(cols, rows);
            caps::test_override::with_unicode(unicode, || {
                caps::test_override::with_color(depth, || {
                    Board::default().paint(&view, &mut surface, Rect::full(cols, rows), 0, &[]);
                })
            });
            let rendered = lines(&surface).join("\n");
            assert!(rendered.contains("Network throughput"));
            assert!(rendered.contains("Interfaces"));
            assert!(rendered.contains("eth"));
            if placed_contains_wifi(&view, Rect::full(cols, rows)) {
                assert!(rendered.contains("Wi-Fi interfaces"));
            }
            let placed = Board::default().placed(&view, Rect::full(cols, rows));
            let expected = placed.iter().fold(CollectorSet::empty(), |mut set, panel| {
                for collector in view.demand(panel.id).iter() {
                    set.insert(collector);
                }
                set
            });
            assert_eq!(
                Board::default()
                    .demand(&view, Rect::full(cols, rows), 1000)
                    .collectors,
                expected
            );
            if unicode == UnicodeLevel::Ascii {
                assert!(rendered.is_ascii());
            }
            if cols == 240 {
                let fill = fill_ratio(&surface, Rect::full(cols, rows));
                // density follow-up (batch 51): estimated, primary sets from the printed value
                assert!(
                    fill >= 0.30,
                    "fill ratio {fill:.3} at {cols}x{rows} {unicode:?}/{depth:?}"
                );
            }
        }
    }
}

fn placed_contains_wifi(view: &NetworkView<'_>, area: Rect) -> bool {
    Board::default()
        .placed(view, area)
        .iter()
        .any(|panel| panel.id == WIFI)
}
