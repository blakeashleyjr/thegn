//! Process column catalogue, deterministic fitting, and cell formatting.
//!
//! This module is intentionally pure so the renderer can share the same column
//! boundaries for every row in a frame.
#![allow(dead_code)] // V-PRC-3 wires this catalogue into the renderer.

use thegn_core::numfmt::{NumFmt, Reading, Unit};
use thegn_core::viz::{self, UnitFmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ColId {
    Pid,
    Ppid,
    User,
    Owner,
    Worktree,
    State,
    Nice,
    Priority,
    Policy,
    Threads,
    Cpu,
    CpuBar,
    CpuHistory,
    Average,
    Time,
    MemPercent,
    Rss,
    Virt,
    Shared,
    Swap,
    MemHistory,
    MinorFaults,
    MajorFaults,
    ReadRate,
    WriteRate,
    IoTotal,
    Fds,
    Delay,
    ContextSwitches,
    CpuNumber,
    Age,
    Tty,
    Oom,
    Unit,
    Container,
    Gpu,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Needs {
    Always,
    User,
    Owner,
    Worktree,
    State,
    Nice,
    Priority,
    Policy,
    Threads,
    Cpu,
    History,
    Average,
    Time,
    Memory,
    Io,
    Fds,
    Delay,
    ContextSwitches,
    CpuNumber,
    Age,
    Tty,
    Oom,
    Unit,
    Container,
    Gpu,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColSpec {
    pub id: ColId,
    pub title: &'static str,
    pub priority: u8,
    pub min: usize,
    pub ideal: usize,
    pub flex: bool,
    pub align: Align,
    pub needs: Needs,
}

macro_rules! col {
    ($id:ident, $title:literal, $priority:literal, $min:literal, $ideal:literal, $flex:literal, $align:ident, $needs:ident) => {
        ColSpec {
            id: ColId::$id,
            title: $title,
            priority: $priority,
            min: $min,
            ideal: $ideal,
            flex: $flex,
            align: Align::$align,
            needs: Needs::$needs,
        }
    };
}

pub(crate) const CATALOG: [ColSpec; 37] = [
    col!(Pid, "PID", 0, 3, 6, false, Right, Always),
    col!(Ppid, "PPID", 8, 4, 6, false, Right, Always),
    col!(User, "USER", 7, 4, 8, false, Left, User),
    col!(Owner, "OWNER", 6, 5, 10, false, Left, Owner),
    col!(Worktree, "WORKTR", 9, 6, 12, false, Left, Worktree),
    col!(State, "S", 1, 1, 1, false, Center, State),
    col!(Nice, "NI", 8, 2, 3, false, Right, Nice),
    col!(Priority, "PRI", 9, 3, 4, false, Right, Priority),
    col!(Policy, "POL", 9, 3, 3, false, Left, Policy),
    col!(Threads, "THR", 8, 3, 4, false, Right, Threads),
    col!(Cpu, "CPU%", 2, 5, 5, false, Right, Cpu),
    col!(CpuBar, "usr/sys", 9, 7, 8, false, Left, Cpu),
    col!(CpuHistory, "CPU~", 9, 4, 8, false, Left, History),
    col!(Average, "AVG", 9, 5, 5, false, Right, Average),
    col!(Time, "TIME+", 8, 8, 8, false, Right, Time),
    col!(MemPercent, "MEM%", 8, 5, 5, false, Right, Memory),
    col!(Rss, "RSS", 3, 5, 5, false, Right, Memory),
    col!(Virt, "VIRT", 9, 5, 5, false, Right, Memory),
    col!(Shared, "SHR", 8, 5, 5, false, Right, Memory),
    col!(Swap, "SWAP", 8, 5, 5, false, Right, Memory),
    col!(MemHistory, "MEM~", 9, 4, 8, false, Left, History),
    col!(MinorFaults, "MINF/s", 9, 6, 6, false, Right, Always),
    col!(MajorFaults, "MAJF/s", 9, 6, 6, false, Right, Always),
    col!(ReadRate, "RD/s", 7, 4, 6, false, Right, Io),
    col!(WriteRate, "WR/s", 7, 4, 6, false, Right, Io),
    col!(IoTotal, "IO tot", 9, 6, 8, false, Right, Io),
    col!(Fds, "FDS", 8, 3, 5, false, Right, Fds),
    col!(Delay, "DLY%", 8, 4, 5, false, Right, Delay),
    col!(
        ContextSwitches,
        "CSW/s",
        8,
        5,
        6,
        false,
        Right,
        ContextSwitches
    ),
    col!(CpuNumber, "CPU#", 9, 4, 4, false, Right, CpuNumber),
    col!(Age, "AGE", 7, 4, 4, false, Right, Age),
    col!(Tty, "TTY", 9, 3, 6, false, Left, Tty),
    col!(Oom, "OOM", 9, 3, 5, false, Right, Oom),
    col!(Unit, "UNIT", 8, 4, 10, false, Left, Unit),
    col!(Container, "CTR", 8, 3, 8, false, Left, Container),
    col!(Gpu, "GPU", 9, 3, 6, false, Right, Gpu),
    col!(Command, "COMMAND", 0, 8, 8, true, Left, Command),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Preset {
    Default,
    Memory,
    Io,
    Sched,
    Thegn,
}

impl Preset {
    pub(crate) const ALL: [Preset; 5] = [
        Preset::Default,
        Preset::Memory,
        Preset::Io,
        Preset::Sched,
        Preset::Thegn,
    ];

    fn columns(self) -> &'static [ColId] {
        use ColId::*;
        match self {
            Preset::Default => &[
                Pid, User, Owner, State, Nice, Threads, Cpu, CpuBar, CpuHistory, MemPercent, Rss,
                Swap, ReadRate, WriteRate, Time, Command,
            ],
            Preset::Memory => &[
                Pid,
                Owner,
                Rss,
                Shared,
                Swap,
                Virt,
                MemPercent,
                MemHistory,
                MajorFaults,
                MinorFaults,
                Oom,
                Command,
            ],
            Preset::Io => &[
                Pid, Owner, ReadRate, WriteRate, IoTotal, Fds, Delay, State, Command,
            ],
            Preset::Sched => &[
                Pid,
                State,
                Nice,
                Priority,
                Policy,
                Threads,
                Cpu,
                Delay,
                ContextSwitches,
                CpuNumber,
                Age,
                Command,
            ],
            Preset::Thegn => &[
                Pid, Owner, Worktree, Unit, Container, Cpu, Average, Rss, Age, Command,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FitFlags {
    pub tree: bool,
    pub detail_open: bool,
}

/// Return the stable set of columns and their cell widths. The returned widths
/// exclude one-cell inter-column gutters; those gutters are accounted for here.
pub(crate) fn fit_columns(preset: Preset, width: usize, flags: FitFlags) -> Vec<(ColId, usize)> {
    use ColId::{Command, Cpu, Pid, Rss};
    if width < 40 {
        return special_fit(width, &[Pid, Command], flags);
    }
    if width < 60 {
        return special_fit(width, &[Pid, Cpu, Rss, Command], flags);
    }

    let columns = preset.columns();
    let mut chosen = vec![Pid];
    let mut candidates: Vec<ColId> = columns
        .iter()
        .copied()
        .filter(|id| *id != Pid && *id != Command)
        .collect();
    // CPU% and RSS are injected into presets that omit them so the narrow-table
    // layout remains a subset when the 60-cell preset layout takes over.
    for id in [Cpu, Rss] {
        if !candidates.contains(&id) {
            candidates.push(id);
        }
    }
    let mut ordered: Vec<ColId> = candidates;
    // The collapse order defines the greedy order directly: latest-to-collapse
    // columns are considered first, independent of the catalogue priority.
    ordered.sort_by_key(|id| std::cmp::Reverse(collapse_rank(*id)));
    // `width` is the whole table viewport. Tree mode reserves two cells for
    // its branch prefix; an open detail pane reserves one for the divider.
    let overhead = usize::from(flags.detail_open) + if flags.tree { 2 } else { 0 };
    let mut used = spec(Pid).min + 1 + overhead; // COMMAND always follows
    for id in ordered {
        let need = spec(id).min + 1;
        if used + need + spec(Command).min <= width {
            chosen.push(id);
            used += need;
        } else {
            break;
        }
    }
    // Render in preset order. Injected columns have a stable explicit position
    // immediately after PID rather than falling back to catalogue indexes.
    let mut render_order = Vec::with_capacity(columns.len() + 2);
    for id in columns {
        render_order.push(*id);
        if *id == Pid {
            if !columns.contains(&Cpu) {
                render_order.push(Cpu);
            }
            if !columns.contains(&Rss) {
                render_order.push(Rss);
            }
        }
    }
    chosen.sort_by_key(|id| {
        render_order
            .iter()
            .position(|candidate| candidate == id)
            .unwrap_or(usize::MAX)
    });
    chosen.push(Command);
    let gutters = chosen.len().saturating_sub(1);
    let reserved = gutters + usize::from(flags.detail_open) + if flags.tree { 2 } else { 0 };
    let available = width.saturating_sub(reserved);
    let mut out: Vec<(ColId, usize)> = chosen.iter().map(|id| (*id, spec(*id).min)).collect();
    let min_total: usize = out.iter().map(|(_, n)| *n).sum();
    let mut extra = available.saturating_sub(min_total);
    for (id, n) in &mut out {
        if *id != Command {
            let growth = spec(*id).ideal.saturating_sub(*n).min(extra);
            *n += growth;
            extra -= growth;
        }
    }
    let command = out.last_mut().expect("COMMAND is always present");
    command.1 += extra;
    out
}

/// The collapse list is ordered from the first column to remove to the last.
/// Within a priority bucket, retain later entries first.
fn collapse_rank(id: ColId) -> usize {
    use ColId::*;
    match id {
        Virt => 0,
        Shared => 1,
        CpuNumber => 2,
        Policy => 3,
        Priority => 4,
        Tty => 5,
        Oom => 6,
        Fds => 7,
        Delay => 8,
        ContextSwitches => 9,
        MajorFaults => 10,
        MinorFaults => 11,
        Swap => 12,
        Time => 13,
        Nice => 14,
        Threads => 15,
        User => 16,
        MemHistory => 17,
        WriteRate => 18,
        ReadRate => 19,
        Average => 20,
        CpuHistory => 21,
        MemPercent => 22,
        CpuBar => 23,
        Owner => 24,
        Rss => 25,
        Cpu => 26,
        State => 27,
        Pid => 28,
        Command => 29,
        Ppid | Worktree | Container | Unit | Gpu | IoTotal | Age => 30,
    }
}

fn special_fit(width: usize, ids: &[ColId], flags: FitFlags) -> Vec<(ColId, usize)> {
    let gutters = ids.len().saturating_sub(1);
    let reserved = gutters + usize::from(flags.detail_open) + if flags.tree { 2 } else { 0 };
    let available = width.saturating_sub(reserved);
    let mut out: Vec<(ColId, usize)> = ids.iter().map(|id| (*id, spec(*id).min)).collect();
    let used: usize = out.iter().map(|(_, n)| *n).sum();
    let extra = available.saturating_sub(used);
    let cmd = out.last_mut().expect("COMMAND is always present");
    cmd.1 += extra;
    out
}

fn spec(id: ColId) -> ColSpec {
    *CATALOG
        .iter()
        .find(|col| col.id == id)
        .expect("catalogue is complete")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RowCaps {
    pub has_ppid: bool,
    pub has_user: bool,
    pub has_owner: bool,
    pub has_worktree: bool,
    pub has_state: bool,
    pub has_nice: bool,
    pub has_priority: bool,
    pub has_policy: bool,
    pub has_threads: bool,
    pub has_cpu: bool,
    pub has_cpu_history: bool,
    pub has_mem_history: bool,
    pub has_average: bool,
    pub has_time: bool,
    pub has_mem_percent: bool,
    pub has_rss: bool,
    pub has_virt: bool,
    pub has_shared: bool,
    pub has_swap: bool,
    pub has_io: bool,
    pub has_fds: bool,
    pub has_delay: bool,
    pub has_context_switches: bool,
    pub has_cpu_number: bool,
    pub has_age: bool,
    pub has_tty: bool,
    pub has_oom: bool,
    pub has_unit: bool,
    pub has_container: bool,
    pub has_gpu: bool,
    pub has_command: bool,
    pub denied: bool,
    pub has_faults: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    NotYet,
    Denied,
}

pub(crate) fn availability(id: ColId, caps: &RowCaps) -> Availability {
    let (present, privileged) = match id {
        ColId::Pid => (true, false),
        ColId::Ppid => (caps.has_ppid, false),
        ColId::User => (caps.has_user, false),
        ColId::Owner => (caps.has_owner, false),
        ColId::Worktree => (caps.has_worktree, false),
        ColId::State => (caps.has_state, false),
        ColId::Nice => (caps.has_nice, false),
        ColId::Priority => (caps.has_priority, false),
        ColId::Policy => (caps.has_policy, false),
        ColId::Threads => (caps.has_threads, false),
        ColId::Cpu | ColId::CpuBar => (caps.has_cpu, false),
        ColId::CpuHistory => (caps.has_cpu_history, false),
        ColId::MemHistory => (caps.has_mem_history, false),
        ColId::Average => (caps.has_average, false),
        ColId::Time => (caps.has_time, false),
        ColId::MemPercent => (caps.has_mem_percent, false),
        ColId::Rss => (caps.has_rss, false),
        ColId::Virt => (caps.has_virt, false),
        ColId::Shared => (caps.has_shared, false),
        ColId::Swap => (caps.has_swap, false),
        ColId::MinorFaults | ColId::MajorFaults => (caps.has_faults, false),
        ColId::ReadRate | ColId::WriteRate | ColId::IoTotal => (caps.has_io, true),
        ColId::Fds => (caps.has_fds, true),
        ColId::Delay => (caps.has_delay, false),
        ColId::ContextSwitches => (caps.has_context_switches, false),
        ColId::CpuNumber => (caps.has_cpu_number, false),
        ColId::Age => (caps.has_age, false),
        ColId::Tty => (caps.has_tty, false),
        ColId::Oom => (caps.has_oom, false),
        ColId::Unit => (caps.has_unit, false),
        ColId::Container => (caps.has_container, false),
        ColId::Gpu => (caps.has_gpu, false),
        ColId::Command => (caps.has_command, true),
    };
    if present {
        Availability::Available
    } else if privileged && caps.denied {
        Availability::Denied
    } else {
        Availability::NotYet
    }
}

pub(crate) fn proc_row_caps() -> RowCaps {
    RowCaps {
        has_ppid: true,
        has_owner: true,
        has_cpu: true,
        has_rss: true,
        has_age: true,
        has_command: true,
        ..RowCaps::default()
    }
}

pub(crate) fn format_cpu(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => {
            let rounded = (value * 10.0).round() / 10.0;
            let rendered = if rounded.abs() >= 100.0 {
                format!("{value:.0}")
            } else {
                format!("{value:.1}")
            };
            if rendered.len() > 5 {
                "#####".into()
            } else {
                format!("{rendered:>5}")
            }
        }
        Some(_) | None => NumFmt::default().reading(Reading::Unknown, Unit::CpuCoreSum, 5),
    }
}

pub(crate) fn format_bytes(value: Option<u64>) -> String {
    let fmt = NumFmt {
        units: UnitFmt {
            bytes: viz::ByteBase::Binary,
            ..UnitFmt::RAW
        },
        ..NumFmt::default()
    };
    match value {
        Some(value) => fmt.fmt(value as f64, Unit::Bytes, 5),
        None => fmt.reading(Reading::Unknown, Unit::Bytes, 5),
    }
}

pub(crate) fn format_time_plus(seconds: Option<f64>) -> String {
    match seconds {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => time_plus(seconds),
        Some(_) | None => NumFmt::default().reading(Reading::Unknown, Unit::TimePlus, 8),
    }
}

fn time_plus(seconds: f64) -> String {
    let total = seconds.floor() as u64;
    let rendered = if total < 3600 {
        format!(
            "{:02}:{:02}.{:02}",
            total / 60,
            total % 60,
            (seconds * 100.0).floor() as u64 % 100
        )
    } else if total < 360_000 {
        format!(
            "{:02}h{:02}:{:02}",
            total / 3600,
            total / 60 % 60,
            total % 60
        )
    } else {
        format!("{}d{:02}h", total / 86_400, total / 3600 % 24)
    };
    if rendered.len() > 8 {
        "########".into()
    } else {
        format!("{rendered:>8}")
    }
}

pub(crate) fn format_age(seconds: Option<u64>) -> String {
    let fmt = NumFmt::default();
    match seconds {
        Some(value) => fmt.fmt(value as f64, Unit::Age, 4),
        None => fmt.reading(Reading::Unknown, Unit::Age, 4),
    }
}

#[cfg(test)]
#[path = "procs_columns_tests.rs"]
mod tests;
