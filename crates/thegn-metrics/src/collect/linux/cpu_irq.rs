//! Linux `/proc/softirqs` and `/proc/interrupts` collector.
use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx, delta,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::cpu_irq::{CoreIrqSample, NumericIrqTop, SoftirqTop},
    parse::{
        interrupts::{self, InterruptsTable},
        softirqs::{self, SoftirqTable},
    },
    schedule::Cadence,
};
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct CpuIrqCollector {
    soft_path: PathBuf,
    irq_path: PathBuf,
    soft_file: Option<ProcFile>,
    irq_file: Option<ProcFile>,
    soft: SoftirqTable,
    previous_soft: SoftirqTable,
    irq: InterruptsTable,
    previous_irq: InterruptsTable,
    previous_ms: Option<u64>,
    primed: bool,
    #[cfg(test)]
    reads: usize,
    #[cfg(test)]
    opens: usize,
}
impl CpuIrqCollector {
    pub fn new() -> Self {
        Self::at("/proc/softirqs", "/proc/interrupts")
    }
    pub fn at(soft_path: impl AsRef<Path>, irq_path: impl AsRef<Path>) -> Self {
        Self {
            soft_path: soft_path.as_ref().to_path_buf(),
            irq_path: irq_path.as_ref().to_path_buf(),
            soft_file: None,
            irq_file: None,
            soft: SoftirqTable::default(),
            previous_soft: SoftirqTable::default(),
            irq: InterruptsTable::with_capacity(),
            previous_irq: InterruptsTable::with_capacity(),
            previous_ms: None,
            primed: false,
            #[cfg(test)]
            reads: 0,
            #[cfg(test)]
            opens: 0,
        }
    }
    fn read(&mut self) -> Result<(), CollectError> {
        let soft_file = self.soft_file.as_mut().ok_or(CollectError::Gone)?;
        let soft = &mut self.soft;
        #[cfg(test)]
        {
            self.reads += 1;
        }
        match soft_file
            .read_all()
            .map(|bytes| softirqs::parse(bytes, soft))
        {
            Ok(true) => (),
            Ok(false) => return Err(CollectError::Parse("/proc/softirqs")),
            Err(error) => {
                return Err(if soft_file.needs_reopen() {
                    CollectError::Gone
                } else {
                    error.into()
                });
            }
        }
        let irq_file = self.irq_file.as_mut().ok_or(CollectError::Gone)?;
        let irq = &mut self.irq;
        #[cfg(test)]
        {
            self.reads += 1;
        }
        match irq_file
            .read_all()
            .map(|bytes| interrupts::parse(bytes, irq))
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(CollectError::Parse("/proc/interrupts")),
            Err(error) => Err(if irq_file.needs_reopen() {
                CollectError::Gone
            } else {
                error.into()
            }),
        }
    }
}
impl Default for CpuIrqCollector {
    fn default() -> Self {
        Self::new()
    }
}
impl Collector for CpuIrqCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CpuIrq
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        #[cfg(test)]
        {
            self.opens += 1;
        }
        let soft = match ProcFile::open(&self.soft_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Health::Absent(Absent::NotOnThisOs);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Health::Absent(Absent::NoPermission);
            }
            Err(_) => return Health::Warming.after_error(0),
        };
        #[cfg(test)]
        {
            self.opens += 1;
        }
        let irq = match ProcFile::open(&self.irq_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Health::Absent(Absent::NotOnThisOs);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Health::Absent(Absent::NoPermission);
            }
            Err(_) => return Health::Warming.after_error(0),
        };
        self.soft_file = Some(soft);
        self.irq_file = Some(irq);
        match self.read() {
            Ok(()) => {
                self.primed = false;
                Health::Warming
            }
            Err(_) => {
                self.close();
                Health::Warming.after_error(0)
            }
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        self.read()?;
        let previous_ms = self.previous_ms;
        let part = &mut frame.parts.cpu_irq;
        part.cores.clear();
        part.numeric_top.clear();
        part.softirq_top = [SoftirqTop::default(); 3];
        part.thr_per_s = None;
        part.mce_per_s = None;
        part.dfr_per_s = None;
        let mut soft_order = [0_usize, 1, 2];
        let mut soft_totals = [None; 10];
        if self.primed {
            for (row, total) in soft_totals.iter_mut().enumerate() {
                *total = soft_total_rate(
                    &self.soft,
                    &self.previous_soft,
                    row,
                    previous_ms,
                    ctx.now_ms,
                );
            }
            soft_order.sort_by(|left, right| {
                soft_totals[*right]
                    .partial_cmp(&soft_totals[*left])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for (slot, &row) in soft_order.iter().enumerate() {
                part.softirq_top[slot] = SoftirqTop {
                    name_hash: interrupts::name_hash(softirqs::TYPES[row]),
                    total_per_s: soft_totals[row],
                };
            }
        }
        for &cpu_id in &self.soft.cpu_ids[..self.soft.cpu_count] {
            let mut core = CoreIrqSample {
                id: cpu_id,
                ..CoreIrqSample::default()
            };
            if self.primed {
                for (slot, &row) in soft_order.iter().enumerate() {
                    core.softirq_per_s[slot] = soft_cell_rate(
                        &self.soft,
                        &self.previous_soft,
                        row,
                        cpu_id,
                        previous_ms,
                        ctx.now_ms,
                    );
                }
                core.res_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"RES",
                    cpu_id,
                    previous_ms,
                    ctx.now_ms,
                );
                core.cal_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"CAL",
                    cpu_id,
                    previous_ms,
                    ctx.now_ms,
                );
                core.tlb_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"TLB",
                    cpu_id,
                    previous_ms,
                    ctx.now_ms,
                );
                core.loc_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"LOC",
                    cpu_id,
                    previous_ms,
                    ctx.now_ms,
                );
            }
            part.cores.push(core);
        }
        if self.primed {
            part.thr_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"THR",
                previous_ms,
                ctx.now_ms,
            );
            part.mce_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"MCE",
                previous_ms,
                ctx.now_ms,
            );
            part.dfr_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"DFR",
                previous_ms,
                ctx.now_ms,
            );
            for row in &self.irq.numeric {
                let Some(previous) = self
                    .previous_irq
                    .numeric
                    .iter()
                    .find(|old| old.irq == row.irq)
                else {
                    continue;
                };
                if let Some(per_s) = irq_row_total_rate(
                    &self.irq,
                    &self.previous_irq,
                    row,
                    previous,
                    previous_ms,
                    ctx.now_ms,
                ) {
                    insert_top(
                        &mut part.numeric_top,
                        NumericIrqTop {
                            irq: row.irq,
                            per_s: Some(per_s),
                        },
                    );
                }
            }
        }
        self.previous_soft = self.soft.clone();
        std::mem::swap(&mut self.irq, &mut self.previous_irq);
        self.previous_ms = Some(ctx.now_ms);
        self.primed = true;
        frame.sampled.insert(CollectorId::CpuIrq);
        Ok(())
    }
    fn close(&mut self) {
        self.soft_file = None;
        self.irq_file = None;
        self.previous_ms = None;
        self.primed = false;
        self.previous_soft = SoftirqTable::default();
    }
}
fn delta32(current: u32, previous: u32) -> Option<u64> {
    if current >= previous {
        Some(u64::from(current - previous))
    } else if previous > 0xC000_0000 {
        Some(u64::from(current.wrapping_sub(previous)))
    } else {
        None
    }
}

fn rate_from_delta(delta: u64, previous_ms: Option<u64>, now_ms: u64) -> Option<f64> {
    delta::rate(delta, 0, previous_ms, now_ms)
}

fn cpu_column(ids: &[u16], cpu_id: u16) -> Option<usize> {
    ids.iter().position(|id| *id == cpu_id)
}

fn soft_cell_rate(
    current: &SoftirqTable,
    old: &SoftirqTable,
    row: usize,
    id: u16,
    then: Option<u64>,
    now: u64,
) -> Option<f64> {
    let current_col = cpu_column(&current.cpu_ids[..current.cpu_count], id)?;
    let old_col = cpu_column(&old.cpu_ids[..old.cpu_count], id)?;
    rate_from_delta(
        delta32(current.counts[row][current_col], old.counts[row][old_col])?,
        then,
        now,
    )
}

fn soft_total_rate(
    current: &SoftirqTable,
    old: &SoftirqTable,
    row: usize,
    then: Option<u64>,
    now: u64,
) -> Option<f64> {
    let mut delta = 0_u64;
    for (col, &id) in current.cpu_ids[..current.cpu_count].iter().enumerate() {
        let old_col = cpu_column(&old.cpu_ids[..old.cpu_count], id)?;
        delta = delta.checked_add(delta32(current.counts[row][col], old.counts[row][old_col])?)?;
    }
    rate_from_delta(delta, then, now)
}
fn named_cell_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    name: &[u8],
    cpu_id: u16,
    previous_ms: Option<u64>,
    now_ms: u64,
) -> Option<f64> {
    let hash = interrupts::name_hash(name);
    let current_row = current.named.iter().find(|row| row.hash == hash)?;
    let old_row = previous.named.iter().find(|row| row.hash == hash)?;
    let current_col = cpu_column(&current.cpu_ids[..current.cpu_count], cpu_id)?;
    let old_col = cpu_column(&previous.cpu_ids[..previous.cpu_count], cpu_id)?;
    rate_from_delta(
        delta32(
            *current_row.counts.get(current_col)?,
            *old_row.counts.get(old_col)?,
        )?,
        previous_ms,
        now_ms,
    )
}
fn named_total_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    name: &[u8],
    previous_ms: Option<u64>,
    now_ms: u64,
) -> Option<f64> {
    let hash = interrupts::name_hash(name);
    let current_row = current.named.iter().find(|row| row.hash == hash)?;
    let old_row = previous.named.iter().find(|row| row.hash == hash)?;
    irq_named_total_rate(current, previous, current_row, old_row, previous_ms, now_ms)
}

fn irq_named_total_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    row: &interrupts::NamedRow,
    old: &interrupts::NamedRow,
    then: Option<u64>,
    now: u64,
) -> Option<f64> {
    irq_columns_total_rate(current, previous, &row.counts, &old.counts, then, now)
}

fn irq_row_total_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    row: &interrupts::IrqRow,
    old: &interrupts::IrqRow,
    then: Option<u64>,
    now: u64,
) -> Option<f64> {
    irq_columns_total_rate(current, previous, &row.counts, &old.counts, then, now)
}

fn irq_columns_total_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    counts: &[u32],
    old_counts: &[u32],
    then: Option<u64>,
    now: u64,
) -> Option<f64> {
    let mut delta = 0_u64;
    for (col, &id) in current.cpu_ids[..current.cpu_count].iter().enumerate() {
        let old_col = cpu_column(&previous.cpu_ids[..previous.cpu_count], id)?;
        delta = delta.checked_add(delta32(*counts.get(col)?, *old_counts.get(old_col)?)?)?;
    }
    rate_from_delta(delta, then, now)
}
fn insert_top(top: &mut Vec<NumericIrqTop>, value: NumericIrqTop) {
    let index = top
        .iter()
        .position(|item| item.per_s.unwrap_or(0.0) < value.per_s.unwrap_or(0.0))
        .unwrap_or(top.len());
    if index < 5 {
        top.insert(index, value);
        if top.len() > 5 {
            top.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CpuIrqCollector;
    use crate::collect::parse::interrupts;
    use crate::collect::{Collector, TickCtx, env::Env, frame::MonitorFrame, health::Health};
    use std::{fs, os::unix::fs::FileExt};
    #[test]
    fn held_files_prime_then_publish_rates_and_backwards_cells_are_none() {
        let dir = tempfile::tempdir().unwrap();
        let soft = dir.path().join("softirqs");
        let irq = dir.path().join("interrupts");
        fs::write(&soft, b" CPU0 CPU1\nTIMER: 10 20\nNET_RX: 20 40\n").unwrap();
        fs::write(&irq, b" CPU0 CPU1\nRES: 10 20\nCAL: 10 20\nTLB: 10 20\nLOC: 10 20\nTHR: 1 2\nMCE: 1 2\nDFR: 1 2\n7: 10 20 device\n").unwrap();
        let mut collector = CpuIrqCollector::at(&soft, &irq);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        assert_eq!(collector.opens, 2);
        assert_eq!(collector.reads, 2);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(collector.reads, 4);
        assert_eq!(collector.opens, 2);
        assert_eq!(frame.parts.cpu_irq.cores[0].res_per_s, None);
        let write = |path: &std::path::Path, bytes: &[u8]| {
            let file = fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_len(0).unwrap();
            file.write_at(bytes, 0).unwrap();
        };
        write(&soft, b" CPU0 CPU1\nTIMER: 20 30\nNET_RX: 25 45\n");
        write(&irq, b" CPU0 CPU1\nRES: 20 30\nCAL: 20 30\nTLB: 20 30\nLOC: 20 30\nTHR: 3 4\nMCE: 3 4\nDFR: 3 4\n7: 20 30 device\n");
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 6001,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(collector.reads, 6);
        assert_eq!(collector.opens, 2);
        assert_eq!(
            frame.parts.cpu_irq.cores[0].res_per_s,
            Some(1.9996000799840032)
        );
        assert_eq!(
            frame.parts.cpu_irq.cores[0].softirq_per_s[0],
            Some(1.9996000799840032)
        );
        assert_eq!(frame.parts.cpu_irq.thr_per_s, Some(0.7998400319936013));
        assert_eq!(
            frame.parts.cpu_irq.numeric_top[0].per_s,
            Some(3.9992001599680064)
        );
        write(&soft, b" CPU0 CPU1\nTIMER: 5 50\nNET_RX: 30 50\n");
        write(&irq, b" CPU0 CPU1\nRES: 30 40\nCAL: 30 40\nTLB: 30 40\nLOC: 30 40\nTHR: 4 5\nMCE: 4 5\nDFR: 4 5\n7: 30 40 device\n");
        collector
            .sample(
                &TickCtx {
                    tick: 3,
                    now_ms: 11002,
                },
                &mut frame,
            )
            .unwrap();
        let timer_slot = frame
            .parts
            .cpu_irq
            .softirq_top
            .iter()
            .position(|top| top.name_hash == interrupts::name_hash(b"TIMER"))
            .unwrap();
        assert_eq!(frame.parts.cpu_irq.cores[0].softirq_per_s[timer_slot], None);
    }

    #[test]
    fn maps_interrupt_columns_to_real_cpu_ids_after_hotplug_gap() {
        let dir = tempfile::tempdir().unwrap();
        let soft = dir.path().join("softirqs");
        let irq = dir.path().join("interrupts");
        fs::write(&soft, b" CPU0 CPU1 CPU2\nTIMER: 1 2 3\n").unwrap();
        fs::write(
            &irq,
            b" CPU0 CPU2\nRES: 10 100\nCAL: 1 1\nTLB: 1 1\nLOC: 1 1\n",
        )
        .unwrap();
        let mut collector = CpuIrqCollector::at(&soft, &irq);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        let write = |path: &std::path::Path, bytes: &[u8]| {
            let file = fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_len(0).unwrap();
            file.write_at(bytes, 0).unwrap();
        };
        write(&soft, b" CPU0 CPU1 CPU2\nTIMER: 2 3 4\n");
        write(
            &irq,
            b" CPU0 CPU2\nRES: 11 102\nCAL: 2 2\nTLB: 2 2\nLOC: 2 2\n",
        );
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .cpu_irq
                .cores
                .iter()
                .map(|core| core.id)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(frame.parts.cpu_irq.cores[1].res_per_s, None);
        assert_eq!(frame.parts.cpu_irq.cores[2].res_per_s, Some(2.0));
    }

    #[test]
    fn wrapped_u32_cells_produce_a_rate() {
        assert_eq!(super::delta32(16, u32::MAX - 15), Some(32));
    }
}
