use super::*;
use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
use crate::{
    caps::{self, Glyph},
    compositor::Rect,
    monitor::board::Board,
};
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId,
    health::{Absent, Health},
    model::{
        cpu_policy::{CpuPolicy, PowerPolicy, SysToken},
        cpu_stat::{CoreSample, ModeShare},
        loadavg::LoadavgPart,
        psi::PsiPart,
    },
    parse::interrupts::name_hash,
    parse::loadavg::Loadavg,
};

const SNAPSHOT_GOLDENS: [&str; 5] = [
    "CPU usage",
    "Cores (L3)",
    "Frequency",
    "Load & pressure",
    "C states residency",
];

fn fixture_frame() -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::CpuStat.index()] = Health::Ok;
    frame.health[CollectorId::CpuFreq.index()] = Health::Ok;
    frame.health[CollectorId::CpuIrq.index()] = Health::Ok;
    frame.health[CollectorId::CpuSched.index()] = Health::Ok;
    frame.health[CollectorId::Loadavg.index()] = Health::Ok;
    frame.health[CollectorId::Psi.index()] = Health::Ok;
    frame.parts.cpu_stat.total = Some(ModeShare {
        busy: 625,
        iowait: 70,
        steal: 25,
        ..ModeShare::default()
    });
    frame.parts.cpu_stat.procs_running = Some(3);
    frame.parts.cpu_stat.cores = (0..24)
        .map(|id| CoreSample {
            id,
            online: true,
            share: Some(ModeShare {
                busy: (id % 8 + 1) * 75,
                iowait: 30,
                steal: 10,
                ..ModeShare::default()
            }),
        })
        .collect();
    frame.parts.cpu_topology.cpus = (0..24)
        .map(
            |id| thegn_metrics::collect::model::cpu_topology::CpuTopologyEntry {
                id,
                core_type: if id < 4 || (12..16).contains(&id) {
                    thegn_metrics::collect::policy::cpu_topology::CoreType::Performance
                } else {
                    thegn_metrics::collect::policy::cpu_topology::CoreType::Efficiency
                },
                online: true,
                ..Default::default()
            },
        )
        .collect();
    frame.parts.cpu_topology.l3_groups = vec![(0..12).collect(), (12..24).collect()];
    frame.parts.cpu_idle.cores = (0..24)
        .map(|cpu| thegn_metrics::collect::model::cpu_idle::CpuIdleCore {
            cpu,
            c0_pct: Some(30.0 + (cpu % 20) as f32 * 2.0),
            states: vec![thegn_metrics::collect::model::cpu_idle::CpuIdleState {
                index: 1,
                name: Some("C1".into()),
                residency_pct: Some(70.0 - (cpu % 20) as f32 * 2.0),
                ..Default::default()
            }],
        })
        .collect();
    frame.health[CollectorId::CpuTopology.index()] = Health::Ok;
    frame.health[CollectorId::CpuIdle.index()] = Health::Ok;
    let governor = SysToken::new(b"schedutil").unwrap();
    let epp = SysToken::new(b"balance_performance").unwrap();
    frame.parts.cpu_freq.policies = vec![
        CpuPolicy {
            policy: 0,
            cur_khz: Some(2_400_000),
            hw_min_khz: Some(800_000),
            hw_max_khz: Some(4_000_000),
            scaling_min_khz: Some(800_000),
            scaling_max_khz: Some(4_000_000),
            governor: Some(governor),
            epp: Some(epp),
            ..CpuPolicy::default()
        },
        CpuPolicy {
            policy: 1,
            cur_khz: Some(1_800_000),
            hw_min_khz: Some(800_000),
            hw_max_khz: Some(3_600_000),
            scaling_min_khz: Some(800_000),
            scaling_max_khz: Some(3_600_000),
            governor: Some(governor),
            epp: Some(epp),
            ..CpuPolicy::default()
        },
    ];
    frame.parts.cpu_freq.power = PowerPolicy {
        platform_profile: SysToken::new(b"balanced"),
        ..PowerPolicy::default()
    };
    frame.parts.cpu_irq.softirq_top[0].name_hash = name_hash(b"TIMER");
    frame.parts.cpu_irq.softirq_top[0].total_per_s = Some(42.0);
    frame
        .parts
        .cpu_irq
        .cores
        .push(thegn_metrics::collect::model::cpu_irq::CoreIrqSample {
            id: 0,
            res_per_s: Some(7.0),
            cal_per_s: Some(4.0),
            tlb_per_s: Some(2.0),
            loc_per_s: Some(3.0),
            ..thegn_metrics::collect::model::cpu_irq::CoreIrqSample::default()
        });
    frame.parts.cpu_sched.version = Some(15);
    frame.parts.cpu_sched.cores = (0..24)
        .map(
            |id| thegn_metrics::collect::model::cpu_sched::CoreSchedSample {
                id,
                online: true,
                run_queue_wait_ms_per_s: Some(f64::from(id) + 0.5),
                avg_wait_ms_per_timeslice: Some(0.2),
            },
        )
        .collect();
    frame.parts.loadavg = LoadavgPart {
        value: Some(Loadavg {
            load1: 325,
            load5: 271,
            load15: 190,
            running: 3,
            total: 24,
            last_pid: 99,
        }),
    };
    frame.parts.psi = PsiPart {
        cpu: thegn_metrics::collect::model::psi::PsiResource {
            present: true,
            some_stall_centi: Some(125),
            ..thegn_metrics::collect::model::psi::PsiResource::default()
        },
        ..PsiPart::default()
    };
    frame
}

fn fixture_history() -> TelemetryHistory {
    let mut history = TelemetryHistory::default();
    for index in 0..120_u64 {
        let snap = thegn_metrics::StatsSnapshot {
            cpu_pct: Some(((index * 17) % 100) as u8),
            load_avg: Some((index as f32 / 10.0, 1.0, 0.5)),
            ..Default::default()
        };
        history.push(&snap, index * 1000);
    }
    history
}

fn sparse_history() -> TelemetryHistory {
    let mut history = TelemetryHistory::default();
    for second in 40..60_u64 {
        let missing_interior_sample = (50..=51).contains(&second);
        let snap = thegn_metrics::StatsSnapshot {
            cpu_pct: (!missing_interior_sample).then_some((40 + second % 20) as u8),
            load_avg: (!missing_interior_sample).then_some((second as f32 / 10.0, 1.0, 0.5)),
            ..Default::default()
        };
        history.push(&snap, second * 1000);
    }
    history
}

fn graph_column_has_ink(surface: &Surface, x: usize) -> bool {
    surface.screen_lines().iter().any(|line| {
        line.visible_cells().any(|cell| {
            cell.cell_index() == x
                && !cell.str().is_empty()
                && cell.str() != " "
                && !cell.str().chars().all(|glyph| glyph as u32 == 0x2800)
        })
    })
}

fn absorb_fixture_core_history(model: &mut CpuModel, frame: &MonitorFrame) {
    for tick in 0..240_u16 {
        let mut sample = frame.clone();
        if let Some(total) = &mut sample.parts.cpu_stat.total {
            total.busy = 450 + tick % 450;
            total.iowait = tick % 100;
            total.steal = (tick * 3) % 80;
        }
        for core in &mut sample.parts.cpu_stat.cores {
            if let Some(share) = &mut core.share {
                share.busy = ((u32::from(core.id) * 31 + u32::from(tick) * 17) % 1000) as u16;
            }
        }
        for policy in &mut sample.parts.cpu_freq.policies {
            if let (Some(min), Some(max)) = (policy.scaling_min_khz, policy.scaling_max_khz) {
                let range = max.saturating_sub(min);
                policy.cur_khz = Some(min + range.saturating_mul(u64::from(tick % 100)) / 100);
            }
        }
        model.absorb_frame(&sample);
    }
    model.absorb_frame(frame);
}

fn lines(surface: &Surface) -> Vec<String> {
    surface
        .screen_lines()
        .iter()
        .map(|line| {
            line.visible_cells()
                .map(|cell| cell.str().to_owned())
                .collect::<String>()
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
            x < x1
                && x.saturating_add(cell.width()) > x0
                && !cell.str().is_empty()
                && cell.str() != " "
                && cell.str() != "\u{2800}"
        })
        .map(|cell| {
            let x = cell.cell_index();
            cell.width()
                .min(x1.saturating_sub(x))
                .min(x.saturating_add(cell.width()).saturating_sub(x0))
        })
        .sum();
    occupied as f32 / cells as f32
}

#[test]
fn absorb_frame_reuses_parts_and_keeps_all_carried_cpu_values() {
    let frame = fixture_frame();
    let mut model = CpuModel::default();
    model.cpu_stat.cores.reserve(24);
    model.cpu_freq.policies.reserve(2);
    model.cpu_irq.cores.reserve(1);
    model.cpu_irq.numeric_top.reserve(5);
    model.cpu_sched.cores.reserve(24);
    let stat_cores = model.cpu_stat.cores.as_ptr();
    let policies = model.cpu_freq.policies.as_ptr();
    let irq_cores = model.cpu_irq.cores.as_ptr();
    let numeric_top = model.cpu_irq.numeric_top.as_ptr();
    let sched_cores = model.cpu_sched.cores.as_ptr();
    model.absorb_frame(&frame);
    assert_eq!(model.cpu_stat.cores.as_ptr(), stat_cores);
    assert_eq!(model.cpu_freq.policies.as_ptr(), policies);
    assert_eq!(model.cpu_irq.cores.as_ptr(), irq_cores);
    assert_eq!(model.cpu_irq.numeric_top.as_ptr(), numeric_top);
    assert_eq!(model.cpu_sched.cores.as_ptr(), sched_cores);
    assert_eq!(model.cpu_stat.cores.len(), 24);
    assert_eq!(
        model.cpu_stat.cores[23].share.map(|share| share.busy),
        Some(600)
    );
    assert_eq!(model.cpu_freq.policies.len(), 2);
    assert_eq!(model.cpu_freq.policies[1].cur_khz, Some(1_800_000));
    assert_eq!(
        model
            .cpu_freq
            .power
            .platform_profile
            .as_ref()
            .map(|v| v.as_str()),
        Some("balanced")
    );
    assert_eq!(
        model
            .cpu_stat
            .total
            .map(|share| (share.iowait, share.steal)),
        Some((70, 25))
    );
    assert_eq!(model.cpu_irq.softirq_top[0].total_per_s, Some(42.0));
    assert_eq!(model.cpu_sched.cores.len(), 24);

    let default_model = CpuModel::default();
    let default_view = CpuView(&default_model);
    assert!(default_view.demand(SCHED).contains(CollectorId::CpuSched));
    let default_placed = Board::default().placed(&default_view, Rect::full(160, 45));
    assert!(default_placed.iter().any(|panel| panel.id == SCHED));

    let mut absent = frame;
    absent.health[CollectorId::CpuSched.index()] = Health::Absent(Absent::NotImplemented);
    let mut model = CpuModel::default();
    model.absorb_frame(&absent);
    let view = CpuView(&model);
    let placed = Board::default().placed(&view, Rect::full(160, 45));
    assert!(!placed.iter().any(|panel| panel.id == SCHED));
    assert!(!view.demand(SCHED).contains(CollectorId::CpuSched));
}

#[test]
fn snapshots_cover_sizes_capabilities_demand_absence_and_fill() {
    let sizes = [(100, 30), (160, 45), (240, 60)];
    let levels = [
        (UnicodeLevel::Full, ColorDepth::Truecolor),
        (UnicodeLevel::Full, ColorDepth::Ansi16),
        (UnicodeLevel::Ascii, ColorDepth::Truecolor),
    ];
    let frame = fixture_frame();
    let mut model = CpuModel::default();
    absorb_fixture_core_history(&mut model, &frame);
    let history = fixture_history();
    model.absorb_history(&history, Window::DEFAULT, ScaleMode::Window, 119_000);
    let view = CpuView(&model);
    for (cols, rows) in sizes {
        for (unicode, depth) in levels {
            let mut surface = Surface::new(cols, rows);
            caps::test_override::with_unicode(unicode, || {
                caps::test_override::with_color(depth, || {
                    Board::default().paint(&view, &mut surface, Rect::full(cols, rows), 0, &[]);
                });
            });
            let rendered = lines(&surface).join("\n");
            let placed = Board::default().placed(&view, Rect::full(cols, rows));
            for golden in SNAPSHOT_GOLDENS {
                let should_render = match golden {
                    "Load & pressure" => placed.iter().any(|panel| panel.id == LOAD),
                    "C states residency" => placed.iter().any(|panel| panel.id.0 == "cpu.idle"),
                    _ => true,
                };
                if should_render {
                    assert!(
                        rendered.contains(golden),
                        "missing {golden} at {cols}x{rows}"
                    );
                }
            }
            let demand = Board::default().demand(&view, Rect::full(cols, rows), 1000);
            let expected = placed.iter().fold(CollectorSet::empty(), |mut set, panel| {
                for id in view.demand(panel.id).iter() {
                    set.insert(id);
                }
                set
            });
            assert_eq!(demand.collectors, expected);
            if unicode == UnicodeLevel::Ascii {
                assert!(rendered.is_ascii());
                if cols >= 160 {
                    assert!(rendered.lines().any(|line| line.contains(" 1 P L30")));
                }
            }
            if cols == 100 {
                assert!(placed.iter().any(|panel| panel.id == IRQ));
                assert!(placed.iter().any(|panel| panel.id == SCHED));
                assert!(demand.collectors.contains(CollectorId::CpuIrq));
                assert!(demand.collectors.contains(CollectorId::CpuSched));
            }
            if cols >= 160 {
                assert!(placed.iter().any(|panel| panel.id == IRQ));
                assert!(rendered.contains("2400MHz"));
                assert!(rendered.contains("TIMER"));
                assert!(rendered.contains("io 7"));
                assert!(rendered.contains("0 P L30"));
                assert!(rendered.contains("C0 30%"));
            }
            if cols == 240 {
                let r = fill_ratio(&surface, Rect::full(cols, rows));
                assert!(r >= 0.55, "fill ratio {r:.3}");
            }
        }
    }

    let mut absent_frame = MonitorFrame::default();
    absent_frame.health[CollectorId::CpuFreq.index()] = Health::Absent(Absent::NoHardware);
    let mut absent_model = CpuModel::default();
    absent_model.absorb_frame(&absent_frame);
    let absent_view = CpuView(&absent_model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&absent_view, &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = lines(&surface).join("\n");
    assert!(rendered.contains(caps::active_glyphs().emdash));
    assert!(!rendered.contains(caps::glyph(Glyph::Lock)));
    assert!(!rendered.contains("0MHz"));

    absent_frame.health[CollectorId::CpuFreq.index()] = Health::Absent(Absent::NoPermission);
    absent_model.absorb_frame(&absent_frame);
    Board::default().paint(
        &CpuView(&absent_model),
        &mut surface,
        Rect::full(160, 45),
        0,
        &[],
    );
    let rendered = lines(&surface).join("\n");
    assert!(rendered.contains(caps::glyph(Glyph::Lock)));
}

#[test]
fn topology_badges_l3_groups_and_idle_residency_are_frame_driven() {
    let frame = fixture_frame();
    let mut model = CpuModel::default();
    absorb_fixture_core_history(&mut model, &frame);
    let history = fixture_history();
    model.absorb_history(&history, Window::DEFAULT, ScaleMode::Window, 119_000);
    let mut surface = Surface::new(240, 60);
    Board::default().paint(&CpuView(&model), &mut surface, Rect::full(240, 60), 0, &[]);
    let rendered = lines(&surface).join("\n");
    assert!(rendered.contains("0 P L30"));
    assert!(rendered.contains("4 E L30"));
    assert!(rendered.contains("12 P L31"));
    assert!(rendered.contains("C0 30%"));
    assert!(rendered.contains("C1 68%"));

    let mut absent = MonitorFrame::default();
    absent.health[CollectorId::CpuStat.index()] = Health::Ok;
    absent
        .parts
        .cpu_stat
        .cores
        .push(thegn_metrics::collect::model::cpu_stat::CoreSample {
            id: 0,
            online: true,
            share: Some(thegn_metrics::collect::model::cpu_stat::ModeShare {
                busy: 500,
                ..Default::default()
            }),
        });
    absent.health[CollectorId::CpuTopology.index()] = Health::Absent(Absent::NoHardware);
    absent.health[CollectorId::CpuIdle.index()] = Health::Absent(Absent::NoHardware);
    model.absorb_frame(&absent);
    let mut absent_surface = Surface::new(240, 60);
    Board::default().paint(
        &CpuView(&model),
        &mut absent_surface,
        Rect::full(240, 60),
        0,
        &[],
    );
    let rendered = lines(&absent_surface).join("\n");
    assert!(!rendered.contains("0 P"));
    assert!(!rendered.contains("L30"));
    assert!(rendered.contains(caps::active_glyphs().emdash));
    assert!(!rendered.contains("C0 0%"));
}

#[test]
fn topology_idle_and_history_buffers_keep_capacity_after_warmup() {
    let frame = fixture_frame();
    let mut model = CpuModel::default();
    model.absorb_frame(&frame);
    let history = fixture_history();
    model.absorb_history(&history, Window::DEFAULT, ScaleMode::Window, 119_000);
    let caps = (
        (
            model.topology.cpus.capacity(),
            model.topology.l3_groups.capacity(),
            model.idle.cores.capacity(),
            model.cpu_stat.cores.capacity(),
            model
                .idle
                .cores
                .iter()
                .map(|core| core.states.capacity())
                .collect::<Vec<_>>(),
            model.core_busy_history.capacity(),
            model
                .core_busy_history
                .iter()
                .map(|history| history.values.capacity())
                .collect::<Vec<_>>(),
        ),
        (
            model.iowait_history.values.capacity(),
            model.steal_history.values.capacity(),
            model.frequency_history.capacity(),
            model
                .frequency_history
                .iter()
                .map(|history| history.samples.values.capacity())
                .collect::<Vec<_>>(),
            model.cpu_history.capacity(),
            model.load_history.capacity(),
        ),
    );
    for _ in 0..1000 {
        model.absorb_frame(&frame);
        model.absorb_history(&history, Window::DEFAULT, ScaleMode::Window, 119_000);
    }
    assert_eq!(
        (
            (
                model.topology.cpus.capacity(),
                model.topology.l3_groups.capacity(),
                model.idle.cores.capacity(),
                model.cpu_stat.cores.capacity(),
                model
                    .idle
                    .cores
                    .iter()
                    .map(|core| core.states.capacity())
                    .collect::<Vec<_>>(),
                model.core_busy_history.capacity(),
                model
                    .core_busy_history
                    .iter()
                    .map(|history| history.values.capacity())
                    .collect::<Vec<_>>(),
            ),
            (
                model.iowait_history.values.capacity(),
                model.steal_history.values.capacity(),
                model.frequency_history.capacity(),
                model
                    .frequency_history
                    .iter()
                    .map(|history| history.samples.values.capacity())
                    .collect::<Vec<_>>(),
                model.cpu_history.capacity(),
                model.load_history.capacity(),
            ),
        ),
        caps
    );
}

#[test]
fn cpu_and_load_history_skip_leading_gaps_hold_interior_gaps_and_reuse_capacity() {
    let history = sparse_history();
    let mut model = CpuModel::default();
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);

    // With a 60 s window, 240 buckets, and the first real sample at 40 s,
    // the sample starts at bucket floor(40 * 240 / 60) = 160.
    assert_eq!(model.cpu_history.len(), 80);
    assert_eq!(model.load_history.len(), 80);
    assert!(
        model.cpu_history[40..48]
            .iter()
            .all(|value| *value == model.cpu_history[39])
    );
    assert!(
        model.load_history[40..48]
            .iter()
            .all(|value| *value == model.load_history[39])
    );

    let capacity = (model.cpu_history.capacity(), model.load_history.capacity());
    for _ in 0..1000 {
        model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    }
    assert_eq!(
        (model.cpu_history.capacity(), model.load_history.capacity()),
        capacity
    );

    let mut surface = Surface::new(120, 8);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::monitor::graphs::paint_history_graph(
            &model.cpu_history,
            thegn_core::theme::RampId::Util,
            Rect::full(120, 8),
            &mut surface,
        );
    });
    assert!(
        (0..80).all(|x| !graph_column_has_ink(&surface, x)),
        "the first 160 missing buckets must leave the graph's first 80 braille columns blank"
    );
    assert!(
        graph_column_has_ink(&surface, 80),
        "the first CPU sample must begin in the column after the 160 leading buckets"
    );
    assert!(
        graph_column_has_ink(&surface, 119),
        "the newest CPU sample must reach the graph's last inner column"
    );
}

#[test]
fn heatmap_right_edge_shows_the_newest_core_sample() {
    let mut frame = fixture_frame();
    let mut model = CpuModel::default();
    for tick in 0..240 {
        for core in &mut frame.parts.cpu_stat.cores {
            if let Some(share) = &mut core.share {
                share.busy = if tick == 239 { 1000 } else { 0 };
            }
        }
        model.absorb_frame(&frame);
    }
    let view = CpuView(&model);
    let placed = Board::default().placed(&view, Rect::full(240, 60));
    let core_panel = placed.iter().find(|panel| panel.id == CORES).unwrap();
    let graph_x = core_panel.rect.x + 27.min(core_panel.rect.cols.saturating_sub(2));
    let graph_width = core_panel.rect.cols.saturating_sub(28);
    let graph_y = core_panel.rect.y + 1;
    let mut surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        Board::default().paint(&view, &mut surface, Rect::full(240, 60), 0, &[]);
        let newest = lines(&surface)[graph_y]
            .chars()
            .nth(graph_x + graph_width - 1)
            .unwrap();
        assert_eq!(newest, *caps::raster_table().heat.last().unwrap());
    });
}

#[test]
fn unknown_softirq_hash_uses_the_absent_mark() {
    let mut frame = fixture_frame();
    frame.parts.cpu_irq.softirq_top[0].name_hash = 0;
    let mut model = CpuModel::default();
    model.absorb_frame(&frame);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&CpuView(&model), &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = lines(&surface).join("\n");
    assert!(!rendered.contains("softirq #"));
    assert!(rendered.contains(caps::active_glyphs().emdash));
}
