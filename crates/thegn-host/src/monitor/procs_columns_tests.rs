#![allow(dead_code)] // V-PRC-3 renders through it

use super::{
    Availability, CATALOG, ColId, FitFlags, Preset, RowCaps, availability, fit_columns, format_age,
    format_bytes, format_cpu, format_time_plus, proc_row_caps,
};

#[test]
fn catalogue_has_all_columns_in_spec_order() {
    let ids: Vec<ColId> = CATALOG.iter().map(|col| col.id).collect();
    assert_eq!(
        ids,
        vec![
            ColId::Pid,
            ColId::Ppid,
            ColId::User,
            ColId::Owner,
            ColId::Worktree,
            ColId::State,
            ColId::Nice,
            ColId::Priority,
            ColId::Policy,
            ColId::Threads,
            ColId::Cpu,
            ColId::CpuBar,
            ColId::CpuHistory,
            ColId::Average,
            ColId::Time,
            ColId::MemPercent,
            ColId::Rss,
            ColId::Virt,
            ColId::Shared,
            ColId::Swap,
            ColId::MemHistory,
            ColId::MinorFaults,
            ColId::MajorFaults,
            ColId::ReadRate,
            ColId::WriteRate,
            ColId::IoTotal,
            ColId::Fds,
            ColId::Delay,
            ColId::ContextSwitches,
            ColId::CpuNumber,
            ColId::Age,
            ColId::Tty,
            ColId::Oom,
            ColId::Unit,
            ColId::Container,
            ColId::Gpu,
            ColId::Command,
        ]
    );
    assert!(
        CATALOG
            .iter()
            .all(|col| col.title.is_ascii() && col.title.len() <= 7)
    );
}

#[test]
fn narrow_width_golden_tables_cover_39_40_59_and_60() {
    for preset in Preset::ALL {
        assert_eq!(
            fit_columns(preset, 39, FitFlags::default()),
            vec![(ColId::Pid, 3), (ColId::Command, 35)]
        );
        assert_eq!(
            fit_columns(preset, 40, FitFlags::default()),
            vec![
                (ColId::Pid, 3),
                (ColId::Cpu, 5),
                (ColId::Rss, 5),
                (ColId::Command, 24),
            ]
        );
        assert_eq!(
            fit_columns(preset, 59, FitFlags::default()),
            vec![
                (ColId::Pid, 3),
                (ColId::Cpu, 5),
                (ColId::Rss, 5),
                (ColId::Command, 43),
            ]
        );
        let at_60 = fit_columns(preset, 60, FitFlags::default());
        assert_eq!(at_60.first().map(|item| item.0), Some(ColId::Pid));
        assert_eq!(at_60.last().map(|item| item.0), Some(ColId::Command));
    }
}

#[test]
fn fit_columns_stays_in_width_and_command_is_last() {
    for preset in Preset::ALL {
        for tree in [false, true] {
            for detail_open in [false, true] {
                let flags = FitFlags { tree, detail_open };
                for width in 20..=300 {
                    let fitted = fit_columns(preset, width, flags);
                    let occupied = fitted.iter().map(|(_, cells)| cells).sum::<usize>()
                        + fitted.len().saturating_sub(1);
                    assert!(occupied <= width);
                    assert_eq!(fitted.last().map(|item| item.0), Some(ColId::Command));
                    assert!(fitted.iter().all(|(_, cells)| *cells > 0));
                }
            }
        }
    }
}

#[test]
fn kept_columns_and_boundaries_are_monotone_for_each_preset() {
    for preset in Preset::ALL {
        let mut previous = Vec::new();
        for width in 20..=300 {
            let fitted = fit_columns(preset, width, FitFlags::default());
            let ids: Vec<ColId> = fitted.iter().map(|item| item.0).collect();
            for id in &previous {
                assert!(
                    ids.contains(id),
                    "{preset:?} dropped {id:?} at width {width}"
                );
            }
            previous = ids;
        }
    }
}

#[test]
fn availability_distinguishes_sampled_missing_and_denied_values() {
    let caps = proc_row_caps();
    assert_eq!(availability(ColId::Pid, &caps), Availability::Available);
    assert_eq!(availability(ColId::Cpu, &caps), Availability::Available);
    assert_eq!(availability(ColId::Rss, &caps), Availability::Available);
    assert_eq!(availability(ColId::MemPercent, &caps), Availability::NotYet);
    assert_eq!(availability(ColId::User, &caps), Availability::NotYet);
    assert_eq!(availability(ColId::ReadRate, &caps), Availability::NotYet);
    let denied = RowCaps {
        denied: true,
        ..caps
    };
    assert_eq!(availability(ColId::ReadRate, &denied), Availability::Denied);
}

#[test]
fn formatters_keep_fixed_widths_and_unknowns() {
    for value in [0.0, 99.9, 100.0, 2400.0] {
        assert_eq!(format_cpu(Some(value)).len(), 5);
    }
    assert_eq!(format_cpu(Some(0.0)), "  0.0");
    assert_eq!(format_cpu(Some(100.0)), "  100");
    assert_eq!(format_cpu(None), "    —");
    for value in [0, 4096, 512 * 1024 * 1024_u64, 12_400_000_000] {
        assert_eq!(format_bytes(Some(value)).len(), 5);
    }
    assert_eq!(format_bytes(None), "    —");
    for value in [0.0, 3.2, 3601.0, 360_001.0] {
        assert_eq!(format_time_plus(Some(value)).len(), 8);
    }
    assert_eq!(format_time_plus(Some(3.2)), "00:03.20");
    assert_eq!(format_time_plus(Some(3601.0)), "01h00:01");
    assert_eq!(format_time_plus(None), "       —");
    assert_eq!(format_age(Some(300)), "  5m");
    assert_eq!(format_age(None), "   —");
}
