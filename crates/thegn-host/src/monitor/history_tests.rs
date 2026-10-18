use super::{FAMILY_CPU_STAT, FAMILY_MASTHEAD, FAMILY_PSI, MonitorHistory, TOTAL_INSTANCE};
use crate::telemetry::{Metric, TelemetryHistory, Window};
use thegn_core::series_store::{CAP_B, SeriesBuf, SeriesId, SeriesKey, Tier};
use thegn_metrics::StatsSnapshot;
use thegn_metrics::collect::{
    COLLECTORS, CollectorId, CollectorSet,
    frame::{MonitorFrame, Parts},
    health::Health,
    model::{
        cpu_stat::{CoreSample, ModeShare},
        loadavg::LoadavgPart,
        psi::PsiPart,
    },
    parse::{
        loadavg::Loadavg,
        psi::{PsiFile, PsiLine},
    },
};

fn key(family: u8, metric: u16, instance: u16) -> SeriesKey {
    SeriesKey {
        family,
        metric,
        instance,
    }
}

fn successful_health(ids: &[CollectorId]) -> [Health; COLLECTORS] {
    let mut health = [Health::default(); COLLECTORS];
    for id in ids {
        health[id.index()] = Health::Ok;
    }
    health
}

fn read(
    history: &MonitorHistory,
    id: SeriesId,
    tier: Tier,
    from: u64,
    to: u64,
) -> Vec<thegn_core::series_store::SeriesBufRow> {
    let mut out = SeriesBuf::default();
    assert!(history.store().read(id, tier, from, to, &mut out));
    out.rows
}

fn frame_with_load(load1: u32, sampled: bool) -> MonitorFrame {
    let mut collectors = CollectorSet::empty();
    if sampled {
        collectors.insert(CollectorId::Loadavg);
    }
    MonitorFrame {
        sampled: collectors,
        health: if sampled {
            successful_health(&[CollectorId::Loadavg])
        } else {
            [Health::default(); COLLECTORS]
        },
        parts: Parts {
            loadavg: LoadavgPart {
                value: Some(Loadavg {
                    load1,
                    ..Loadavg::default()
                }),
            },
            ..Parts::default()
        },
        ..MonitorFrame::default()
    }
}

fn frame_with_core(core_id: u16, busy: Option<u16>, online: bool) -> MonitorFrame {
    let mut sampled = CollectorSet::empty();
    sampled.insert(CollectorId::CpuStat);
    MonitorFrame {
        sampled,
        health: successful_health(&[CollectorId::CpuStat]),
        parts: Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                total: Some(ModeShare {
                    busy: 500,
                    ..ModeShare::default()
                }),
                cores: vec![CoreSample {
                    id: core_id,
                    online,
                    share: busy.map(|busy| ModeShare {
                        busy,
                        ..ModeShare::default()
                    }),
                }],
                ..Default::default()
            },
            ..Parts::default()
        },
        ..MonitorFrame::default()
    }
}

fn snapshot(load: f32, cpu: Option<u8>) -> StatsSnapshot {
    StatsSnapshot {
        load_avg: Some((load, load, load)),
        cpu_pct: cpu,
        ..StatsSnapshot::default()
    }
}

#[test]
fn masthead_covers_a_simulated_day_in_t2_and_last_hour_in_t1() {
    let mut history = MonitorHistory::new();
    let start = 1_760_000_017_137_u64;
    for tick in 0_u64..43_200 {
        let at = start + tick * 2_000 + 3;
        history.ingest_masthead(&snapshot(1.5, Some(42)), at);
    }
    history.ingest_masthead(&snapshot(1.5, Some(42)), start + 86_400_003);
    history.ingest_masthead(&snapshot(1.5, Some(42)), start + 86_401_003);
    let id = history
        .id_for(key(FAMILY_MASTHEAD, Metric::Cpu.index() as u16, 0))
        .unwrap();
    let now = start + 86_400_000;
    assert_ne!(start % 60_000, 0);
    let expected_covered_secs = ((now - (start + 3)) / 1_000 + 1) as u32;
    assert_eq!(
        history.store().covered_secs(id, now),
        Some(expected_covered_secs)
    );
    let day = read(&history, id, Tier::T2, now - 86_400_000, now);
    assert_eq!(day.len() as u64, now / 60_000 - (now - 86_400_000) / 60_000);
    assert!(day.iter().all(|row| row.mean.is_finite()));
    let hour = read(&history, id, Tier::T1, now - 3_600_000, now);
    assert_eq!(hour.len() as u64, now / 5_000 - (now - 3_600_000) / 5_000);
    assert!(hour.iter().all(|row| row.mean.is_finite()));
}

#[test]
fn a_ten_minute_ticker_stall_marks_exact_missing_buckets() {
    let mut history = MonitorHistory::new();
    for second in 0_u64..600 {
        history.ingest_masthead(&snapshot(1.0, Some(50)), second * 1_000);
    }
    for second in 1_200_u64..1_261 {
        history.ingest_masthead(&snapshot(1.0, Some(50)), second * 1_000);
    }
    let id = history
        .id_for(key(FAMILY_MASTHEAD, Metric::Cpu.index() as u16, 0))
        .unwrap();
    let rows = read(&history, id, Tier::T1, 0, 1_261_000);
    let missing = rows.iter().filter(|row| row.mean.is_nan()).count();
    assert_eq!(missing, 120);
    assert!(rows.first().is_some_and(|row| row.mean.is_finite()));
    assert!(rows.last().is_some_and(|row| row.mean.is_finite()));
    let t2 = read(&history, id, Tier::T2, 0, 1_261_000);
    assert_eq!(t2.iter().filter(|row| row.mean.is_nan()).count(), 10);
    assert!(t2.first().is_some_and(|row| row.mean.is_finite()));
    assert!(t2.last().is_some_and(|row| row.mean.is_finite()));
}

#[test]
fn frame_load_wins_its_second_in_either_arrival_order_and_only_once_in_t1() {
    for frame_first in [false, true] {
        let mut history = MonitorHistory::new();
        history.ingest_masthead(&snapshot(1.0, None), 0);
        if frame_first {
            history.ingest_frame(&frame_with_load(200, true), 1_000);
            history.ingest_masthead(&snapshot(1.0, None), 1_001);
        } else {
            history.ingest_masthead(&snapshot(1.0, None), 1_001);
            history.ingest_frame(&frame_with_load(200, true), 1_000);
        }
        for second in 2_u64..6 {
            history.ingest_masthead(&snapshot(1.0, None), second * 1_000);
        }
        let id = history
            .id_for(key(FAMILY_MASTHEAD, Metric::Load.index() as u16, 0))
            .unwrap();
        let t0 = read(&history, id, Tier::T0, 1_000, 1_000);
        assert_eq!(t0[0].mean, 2.0);
        let t1 = read(&history, id, Tier::T1, 0, 5_000);
        assert_eq!(t1[0].mean, (2.0 + 1.0 + 1.0 + 1.0 + 1.0) / 5.0);
        history.ingest_masthead(&snapshot(3.0, None), 6_000);
        let next = read(&history, id, Tier::T0, 6_000, 6_000);
        assert_eq!(next[0].mean, 3.0);
    }
}

#[test]
fn unsampled_loadavg_does_not_push_or_claim_a_second() {
    let mut history = MonitorHistory::new();
    history.ingest_frame(&frame_with_load(200, false), 10_000);
    history.ingest_masthead(&snapshot(1.25, None), 10_001);
    let id = history
        .id_for(key(FAMILY_MASTHEAD, Metric::Load.index() as u16, 0))
        .unwrap();
    assert_eq!(read(&history, id, Tier::T0, 10_000, 10_000)[0].mean, 1.25);
}

#[test]
fn failed_loadavg_attempt_does_not_push_or_claim_the_second() {
    let mut history = MonitorHistory::new();
    history.ingest_masthead(&snapshot(1.0, None), 10_000);
    let mut failed = frame_with_load(200, true);
    failed.health[CollectorId::Loadavg.index()] = Health::Failing {
        since_tick: 4,
        backoff: 1,
        retry_at: 5,
    };
    history.ingest_frame(&failed, 10_001);
    history.ingest_masthead(&snapshot(1.25, None), 10_002);

    let id = history
        .id_for(key(FAMILY_MASTHEAD, Metric::Load.index() as u16, 0))
        .unwrap();
    assert_eq!(read(&history, id, Tier::T0, 10_000, 10_000)[0].mean, 1.25);
}

#[test]
fn psi_and_cpu_stat_parts_map_to_class_a_and_b_series() {
    let mut sampled = CollectorSet::empty();
    sampled.insert(CollectorId::Psi);
    sampled.insert(CollectorId::CpuStat);
    let frame = MonitorFrame {
        sampled,
        health: successful_health(&[CollectorId::Psi, CollectorId::CpuStat]),
        parts: Parts {
            psi: PsiPart {
                cpu: thegn_metrics::collect::model::psi::PsiResource {
                    present: true,
                    file: PsiFile {
                        some: Some(PsiLine {
                            avg10: 125,
                            ..PsiLine::default()
                        }),
                        full: None,
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                total: Some(ModeShare {
                    busy: 425,
                    ..ModeShare::default()
                }),
                cores: vec![CoreSample {
                    id: 7,
                    online: true,
                    share: Some(ModeShare {
                        busy: 650,
                        ..ModeShare::default()
                    }),
                }],
                ..Default::default()
            },
            ..Parts::default()
        },
        ..MonitorFrame::default()
    };
    let mut history = MonitorHistory::new();
    history.ingest_frame(&frame, 4_000);
    assert_eq!(
        read(
            &history,
            history.id_for(key(FAMILY_PSI, 0, 0)).unwrap(),
            Tier::T0,
            4_000,
            4_000
        )[0]
        .mean,
        1.25
    );
    assert_eq!(
        read(
            &history,
            history
                .id_for(key(FAMILY_CPU_STAT, 0, TOTAL_INSTANCE))
                .unwrap(),
            Tier::T0,
            4_000,
            4_000
        )[0]
        .mean,
        42.5
    );
    assert_eq!(
        read(
            &history,
            history.id_for(key(FAMILY_CPU_STAT, 0, 7)).unwrap(),
            Tier::T0,
            4_000,
            4_000
        )[0]
        .mean,
        65.0
    );
}

#[test]
fn pause_leaves_core_gaps_while_masthead_keeps_filling() {
    let mut history = MonitorHistory::new();
    for second in 0_u64..60 {
        history.ingest_masthead(&snapshot(1.0, Some(50)), second * 1_000);
        history.ingest_frame(&frame_with_core(2, Some(500), true), second * 1_000);
    }
    for second in 60_u64..90 {
        history.ingest_masthead(&snapshot(1.0, Some(50)), second * 1_000);
    }
    history.ingest_masthead(&snapshot(1.0, Some(50)), 90_000);
    history.ingest_frame(&frame_with_core(2, Some(500), true), 90_000);
    let core = history.id_for(key(FAMILY_CPU_STAT, 0, 2)).unwrap();
    let masthead = history
        .id_for(key(FAMILY_MASTHEAD, Metric::Cpu.index() as u16, 0))
        .unwrap();
    let core_rows = read(&history, core, Tier::T0, 0, 90_000);
    assert_eq!(core_rows.iter().filter(|row| row.mean.is_nan()).count(), 30);
    let masthead_rows = read(&history, masthead, Tier::T0, 0, 90_000);
    assert!(masthead_rows.iter().all(|row| row.mean.is_finite()));
}

#[test]
fn capped_core_ids_are_refused_once_and_never_panic() {
    let mut history = MonitorHistory::new();
    let mut sampled = CollectorSet::empty();
    sampled.insert(CollectorId::CpuStat);
    let cores: Vec<_> = (0_u16..600)
        .rev()
        .map(|id| CoreSample {
            id,
            online: true,
            share: Some(ModeShare {
                busy: 500,
                ..ModeShare::default()
            }),
        })
        .collect();
    let frame = MonitorFrame {
        sampled,
        health: successful_health(&[CollectorId::CpuStat]),
        parts: Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                cores,
                ..Default::default()
            },
            ..Parts::default()
        },
        ..MonitorFrame::default()
    };
    history.ingest_frame(&frame, 1_000);
    assert_eq!(history.core_len, CAP_B);
    assert!(history.refused_interns > 0);
    let scans = history.intern_scans;
    history.ingest_frame(&frame, 2_000);
    assert_eq!(history.intern_scans, scans);
}

#[test]
fn warmed_masthead_and_frame_ingest_allocate_nothing() {
    let mut history = MonitorHistory::new();
    let snap = snapshot(1.0, Some(30));
    let frame = frame_with_core(1, Some(500), true);
    history.ingest_masthead(&snap, 1_000);
    history.ingest_frame(&frame, 1_000);
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        history.ingest_masthead(&snap, 2_000);
        history.ingest_frame(&frame, 2_000);
    });
    assert_eq!(counts.allocation_calls, 0);
}

#[test]
fn masthead_values_match_telemetry_history_for_every_metric() {
    let mut monitor = MonitorHistory::new();
    let mut telemetry = TelemetryHistory::default();
    for second in 0_u64..12 {
        let snap = StatsSnapshot {
            cpu_pct: Some((second % 100) as u8),
            mem_gib: Some((2.0 + second as f32, 8.0)),
            load_avg: Some((second as f32, 0.0, 0.0)),
            ..StatsSnapshot::default()
        };
        let at = 1_337 + second * 1_000;
        monitor.ingest_masthead(&snap, at);
        telemetry.push(&snap, at);
    }
    for metric in Metric::ALL {
        let expected = telemetry.last_raw(metric);
        let id = monitor.id_for(key(FAMILY_MASTHEAD, metric.index() as u16, 0));
        let actual = id.map_or(f32::NAN, |id| {
            let rows = read(&monitor, id, Tier::T0, 12_337, 12_337);
            rows.last().map_or(f32::NAN, |row| row.mean)
        });
        assert!(
            actual == expected || (actual.is_nan() && expected.is_nan()),
            "{metric:?}: {actual} != {expected}"
        );
        if let Some(id) = id {
            assert_eq!(monitor.store().covered_secs(id, 12_337), Some(12));
        }
    }
    let telemetry_coverage = telemetry
        .coverage_secs(12_337, Window::from_secs(60))
        .expect("short history has a reported coverage");
    let monitor_coverage = monitor
        .id_for(key(FAMILY_MASTHEAD, Metric::Cpu.index() as u16, 0))
        .and_then(|id| monitor.store().covered_secs(id, 12_337))
        .expect("CPU series is populated") as f32;
    assert!((monitor_coverage - telemetry_coverage).abs() <= 1.0);
}

#[test]
fn metrics_that_have_never_reported_are_not_interned() {
    let mut history = MonitorHistory::new();
    history.ingest_masthead(&StatsSnapshot::default(), 1_000);
    for metric in Metric::ALL {
        assert!(
            history
                .id_for(key(FAMILY_MASTHEAD, metric.index() as u16, 0))
                .is_none()
        );
    }
    assert!(history.id_for(key(FAMILY_PSI, 0, 0)).is_none());
}

#[test]
fn psi_absence_records_nan_and_wall_clock_is_unix_milliseconds() {
    let mut sampled = CollectorSet::empty();
    sampled.insert(CollectorId::Psi);
    let present = MonitorFrame {
        sampled,
        health: successful_health(&[CollectorId::Psi]),
        parts: Parts {
            psi: PsiPart {
                cpu: thegn_metrics::collect::model::psi::PsiResource {
                    present: true,
                    file: PsiFile {
                        some: Some(PsiLine {
                            avg10: 100,
                            ..PsiLine::default()
                        }),
                        ..PsiFile::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Parts::default()
        },
        ..MonitorFrame::default()
    };
    let mut history = MonitorHistory::new();
    history.ingest_frame(&present, 3_000);
    history.ingest_frame(
        &MonitorFrame {
            sampled,
            health: successful_health(&[CollectorId::Psi]),
            ..MonitorFrame::default()
        },
        4_000,
    );
    let id = history.id_for(key(FAMILY_PSI, 0, 0)).unwrap();
    assert!(read(&history, id, Tier::T0, 4_000, 4_000)[0].mean.is_nan());
    assert!(super::wall_now_ms() > 1_000_000_000_000);
}
