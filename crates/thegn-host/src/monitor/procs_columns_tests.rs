#![allow(dead_code)] // V-PRC-3 renders through it

use super::{
    Availability, CATALOG, ColId, FitFlags, Preset, RowCaps, availability, fit_columns, format_age,
    format_bytes, format_cpu, format_time_plus, proc_row_caps,
};

use ColId::*;

const G0: &[(ColId, usize)] = &[(Pid, 3), (Cpu, 5), (Rss, 5), (Command, 24)];
const G1: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 6),
    (State, 1),
    (Cpu, 5),
    (CpuBar, 7),
    (CpuHistory, 4),
    (MemPercent, 5),
    (Rss, 5),
    (ReadRate, 4),
    (Command, 8),
];
const G2: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 10),
    (State, 1),
    (Nice, 2),
    (Threads, 3),
    (Cpu, 5),
    (CpuBar, 7),
    (CpuHistory, 4),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 4),
    (WriteRate, 4),
    (Time, 8),
    (Command, 8),
];
const G3: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 10),
    (State, 1),
    (Nice, 3),
    (Threads, 4),
    (Cpu, 5),
    (CpuBar, 8),
    (CpuHistory, 8),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 6),
    (WriteRate, 6),
    (Time, 8),
    (Command, 57),
];
const G4: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 10),
    (State, 1),
    (Nice, 3),
    (Threads, 4),
    (Cpu, 5),
    (CpuBar, 8),
    (CpuHistory, 8),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 6),
    (WriteRate, 6),
    (Time, 8),
    (Command, 137),
];
const G5: &[(ColId, usize)] = &[(Pid, 3), (Cpu, 5), (Rss, 5), (Command, 22)];
const G6: &[(ColId, usize)] = &[
    (Pid, 5),
    (Owner, 5),
    (State, 1),
    (Cpu, 5),
    (CpuBar, 7),
    (CpuHistory, 4),
    (MemPercent, 5),
    (Rss, 5),
    (ReadRate, 4),
    (Command, 8),
];
const G7: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 8),
    (State, 1),
    (Nice, 2),
    (Threads, 3),
    (Cpu, 5),
    (CpuBar, 7),
    (CpuHistory, 4),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 4),
    (WriteRate, 4),
    (Time, 8),
    (Command, 8),
];
const G8: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 10),
    (State, 1),
    (Nice, 3),
    (Threads, 4),
    (Cpu, 5),
    (CpuBar, 8),
    (CpuHistory, 8),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 6),
    (WriteRate, 6),
    (Time, 8),
    (Command, 55),
];
const G9: &[(ColId, usize)] = &[
    (Pid, 6),
    (User, 8),
    (Owner, 10),
    (State, 1),
    (Nice, 3),
    (Threads, 4),
    (Cpu, 5),
    (CpuBar, 8),
    (CpuHistory, 8),
    (MemPercent, 5),
    (Rss, 5),
    (Swap, 5),
    (ReadRate, 6),
    (WriteRate, 6),
    (Time, 8),
    (Command, 135),
];
const G10: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 8),
    (Rss, 5),
    (Swap, 5),
    (MemPercent, 5),
    (MemHistory, 4),
    (MinorFaults, 6),
    (Command, 8),
];
const G11: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 17),
];
const G12: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 77),
];
const G13: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 157),
];
const G14: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 6),
    (Rss, 5),
    (Swap, 5),
    (MemPercent, 5),
    (MemHistory, 4),
    (MinorFaults, 6),
    (Command, 8),
];
const G15: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 15),
];
const G16: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 75),
];
const G17: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Owner, 10),
    (Rss, 5),
    (Shared, 5),
    (Swap, 5),
    (Virt, 5),
    (MemPercent, 5),
    (MemHistory, 8),
    (MajorFaults, 6),
    (MinorFaults, 6),
    (Oom, 5),
    (Command, 155),
];
const G18: &[(ColId, usize)] = &[
    (Pid, 5),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 5),
    (ReadRate, 4),
    (WriteRate, 4),
    (IoTotal, 6),
    (Fds, 3),
    (Delay, 4),
    (State, 1),
    (Command, 8),
];
const G19: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 33),
];
const G20: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 93),
];
const G21: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 173),
];
const G22: &[(ColId, usize)] = &[
    (Pid, 3),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 5),
    (ReadRate, 4),
    (WriteRate, 4),
    (IoTotal, 6),
    (Fds, 3),
    (Delay, 4),
    (State, 1),
    (Command, 8),
];
const G23: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 31),
];
const G24: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 91),
];
const G25: &[(ColId, usize)] = &[
    (Pid, 6),
    (Cpu, 5),
    (Rss, 5),
    (Owner, 10),
    (ReadRate, 6),
    (WriteRate, 6),
    (IoTotal, 8),
    (Fds, 5),
    (Delay, 5),
    (State, 1),
    (Command, 171),
];
const G26: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 2),
    (Priority, 3),
    (Policy, 3),
    (Threads, 3),
    (Cpu, 5),
    (Delay, 4),
    (ContextSwitches, 5),
    (Age, 4),
    (Command, 8),
];
const G27: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 38),
];
const G28: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 98),
];
const G29: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 178),
];
const G30: &[(ColId, usize)] = &[
    (Pid, 4),
    (Rss, 5),
    (State, 1),
    (Nice, 2),
    (Priority, 3),
    (Policy, 3),
    (Threads, 3),
    (Cpu, 5),
    (Delay, 4),
    (ContextSwitches, 5),
    (Age, 4),
    (Command, 8),
];
const G31: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 36),
];
const G32: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 96),
];
const G33: &[(ColId, usize)] = &[
    (Pid, 6),
    (Rss, 5),
    (State, 1),
    (Nice, 3),
    (Priority, 4),
    (Policy, 3),
    (Threads, 4),
    (Cpu, 5),
    (Delay, 5),
    (ContextSwitches, 6),
    (CpuNumber, 4),
    (Age, 4),
    (Command, 176),
];
const G34: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 5),
    (Worktree, 6),
    (Unit, 4),
    (Container, 3),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 8),
];
const G35: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 26),
];
const G36: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 86),
];
const G37: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 166),
];
const G38: &[(ColId, usize)] = &[
    (Pid, 4),
    (Owner, 5),
    (Worktree, 6),
    (Unit, 4),
    (Container, 3),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 8),
];
const G39: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 24),
];
const G40: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 84),
];
const G41: &[(ColId, usize)] = &[
    (Pid, 6),
    (Owner, 10),
    (Worktree, 12),
    (Unit, 10),
    (Container, 8),
    (Cpu, 5),
    (Average, 5),
    (Rss, 5),
    (Age, 4),
    (Command, 164),
];

/// (preset, width, tree, expected columns with widths).
type Golden = (Preset, usize, bool, &'static [(ColId, usize)]);

const GOLDENS: &[Golden] = &[
    (Preset::Default, 40, false, G0),
    (Preset::Default, 60, false, G1),
    (Preset::Default, 100, false, G2),
    (Preset::Default, 160, false, G3),
    (Preset::Default, 240, false, G4),
    (Preset::Default, 40, true, G5),
    (Preset::Default, 60, true, G6),
    (Preset::Default, 100, true, G7),
    (Preset::Default, 160, true, G8),
    (Preset::Default, 240, true, G9),
    (Preset::Memory, 40, false, G0),
    (Preset::Memory, 60, false, G10),
    (Preset::Memory, 100, false, G11),
    (Preset::Memory, 160, false, G12),
    (Preset::Memory, 240, false, G13),
    (Preset::Memory, 40, true, G5),
    (Preset::Memory, 60, true, G14),
    (Preset::Memory, 100, true, G15),
    (Preset::Memory, 160, true, G16),
    (Preset::Memory, 240, true, G17),
    (Preset::Io, 40, false, G0),
    (Preset::Io, 60, false, G18),
    (Preset::Io, 100, false, G19),
    (Preset::Io, 160, false, G20),
    (Preset::Io, 240, false, G21),
    (Preset::Io, 40, true, G5),
    (Preset::Io, 60, true, G22),
    (Preset::Io, 100, true, G23),
    (Preset::Io, 160, true, G24),
    (Preset::Io, 240, true, G25),
    (Preset::Sched, 40, false, G0),
    (Preset::Sched, 60, false, G26),
    (Preset::Sched, 100, false, G27),
    (Preset::Sched, 160, false, G28),
    (Preset::Sched, 240, false, G29),
    (Preset::Sched, 40, true, G5),
    (Preset::Sched, 60, true, G30),
    (Preset::Sched, 100, true, G31),
    (Preset::Sched, 160, true, G32),
    (Preset::Sched, 240, true, G33),
    (Preset::Thegn, 40, false, G0),
    (Preset::Thegn, 60, false, G34),
    (Preset::Thegn, 100, false, G35),
    (Preset::Thegn, 160, false, G36),
    (Preset::Thegn, 240, false, G37),
    (Preset::Thegn, 40, true, G5),
    (Preset::Thegn, 60, true, G38),
    (Preset::Thegn, 100, true, G39),
    (Preset::Thegn, 160, true, G40),
    (Preset::Thegn, 240, true, G41),
];

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
            .all(|col| col.title.is_ascii() && col.title.len() <= 7 && col.title.len() <= col.min)
    );
    assert_eq!(CATALOG.iter().find(|col| col.id == Age).unwrap().min, 4);
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
fn preset_fit_goldens_cover_widths_and_tree_reservation() {
    assert_eq!(GOLDENS.len(), Preset::ALL.len() * 5 * 2);
    for (preset, width, tree, expected) in GOLDENS {
        let actual = fit_columns(
            *preset,
            *width,
            FitFlags {
                tree: *tree,
                ..FitFlags::default()
            },
        );
        assert_eq!(
            actual.as_slice(),
            *expected,
            "{preset:?} at width {width} with tree={tree}"
        );
        let occupied = actual.iter().map(|(_, cells)| cells).sum::<usize>()
            + actual.len().saturating_sub(1)
            + if *tree { 2 } else { 0 };
        assert_eq!(
            occupied, *width,
            "{preset:?} at width {width} with tree={tree}: columns, gutters, and reserve must fill the viewport"
        );
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
                        + fitted.len().saturating_sub(1)
                        + usize::from(detail_open)
                        + if tree { 2 } else { 0 };
                    assert_eq!(occupied, width);
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
        for flags in [
            FitFlags::default(),
            FitFlags {
                tree: true,
                ..FitFlags::default()
            },
            FitFlags {
                detail_open: true,
                ..FitFlags::default()
            },
            FitFlags {
                tree: true,
                detail_open: true,
            },
        ] {
            let mut previous = Vec::new();
            for width in 20..=300 {
                let fitted = fit_columns(preset, width, flags);
                let ids: Vec<ColId> = fitted.iter().map(|item| item.0).collect();
                for id in &previous {
                    assert!(
                        ids.contains(id),
                        "{preset:?} {flags:?} dropped {id:?} at width {width}"
                    );
                }
                previous = ids;
            }
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
    for value in [0.0, 99.9, 99.96, 100.0, 2400.0] {
        assert_eq!(format_cpu(Some(value)).len(), 5);
    }
    assert_eq!(format_cpu(Some(0.0)), "  0.0");
    assert_eq!(format_cpu(Some(100.0)), "  100");
    assert_eq!(format_cpu(Some(99.96)), "  100");
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
