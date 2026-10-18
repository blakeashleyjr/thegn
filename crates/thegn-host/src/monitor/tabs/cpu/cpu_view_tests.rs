use super::*;
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

const SNAPSHOT_GOLDENS: [&str; 4] = ["CPU usage", "Cores", "Frequency", "Load & pressure"];

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
    model.absorb_frame(&frame);
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
                if golden != "Load & pressure" || placed.iter().any(|panel| panel.id == LOAD) {
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
            }
            if cols == 100 {
                assert!(placed.len() < SPECS.len());
                assert!(!demand.collectors.contains(CollectorId::CpuIrq));
                assert!(!demand.collectors.contains(CollectorId::CpuSched));
            }
            if cols >= 160 {
                assert!(placed.iter().any(|panel| panel.id == IRQ));
                assert!(rendered.contains("2400MHz"));
                assert!(rendered.contains("TIMER"));
                assert!(rendered.contains("io 7% st 2%"));
            }
            if cols == 240 {
                assert!(fill_ratio(&surface, Rect::full(cols, rows)) >= 0.55);
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
