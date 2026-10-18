//! Linux `/proc/softirqs` and `/proc/interrupts` collector.
use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    delta::Counter,
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
        #[cfg(test)]
        {
            self.reads += 2;
        }
        let soft_file = self.soft_file.as_mut().ok_or(CollectError::Gone)?;
        match soft_file.read_all() {
            Ok(bytes) if softirqs::parse(bytes, &mut self.soft) => (),
            Ok(_) => return Err(CollectError::Parse("/proc/softirqs")),
            Err(_error) if soft_file.needs_reopen() => return Err(CollectError::Gone),
            Err(error) => return Err(error.into()),
        }
        let irq_file = self.irq_file.as_mut().ok_or(CollectError::Gone)?;
        match irq_file.read_all() {
            Ok(bytes) if interrupts::parse(bytes, &mut self.irq) => Ok(()),
            Ok(_) => Err(CollectError::Parse("/proc/interrupts")),
            Err(_error) if irq_file.needs_reopen() => Err(CollectError::Gone),
            Err(error) => Err(error.into()),
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
        #[cfg(test)]
        {
            self.opens += 2;
        }
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
            for row in 0..softirqs::TYPES.len() {
                let mut current_total = 0_u64;
                let mut previous_total = 0_u64;
                for cpu in 0..self.soft.cpu_count {
                    current_total = current_total.saturating_add(self.soft.counts[row][cpu]);
                    previous_total =
                        previous_total.saturating_add(self.previous_soft.counts[row][cpu]);
                }
                soft_totals[row] =
                    counter_rate(current_total, previous_total, previous_ms, ctx.now_ms);
            }
            soft_order.sort_by(|left, right| {
                soft_totals[*right]
                    .partial_cmp(&soft_totals[*left])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for slot in 0..3 {
                let row = soft_order[slot];
                part.softirq_top[slot] = SoftirqTop {
                    name_hash: interrupts::hash_name(softirqs::TYPES[row]),
                    total_per_s: soft_totals[row],
                };
            }
        }
        for cpu in 0..self.soft.cpu_count {
            let mut core = CoreIrqSample {
                id: cpu as u16,
                ..CoreIrqSample::default()
            };
            if self.primed {
                for slot in 0..3 {
                    let row = soft_order[slot];
                    core.softirq_per_s[slot] = counter_rate(
                        self.soft.counts[row][cpu],
                        self.previous_soft.counts[row][cpu],
                        previous_ms,
                        ctx.now_ms,
                    );
                }
                core.res_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"RES",
                    cpu,
                    previous_ms,
                    ctx.now_ms,
                    self.primed,
                );
                core.cal_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"CAL",
                    cpu,
                    previous_ms,
                    ctx.now_ms,
                    self.primed,
                );
                core.tlb_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"TLB",
                    cpu,
                    previous_ms,
                    ctx.now_ms,
                    self.primed,
                );
                core.loc_per_s = named_cell_rate(
                    &self.irq,
                    &self.previous_irq,
                    b"LOC",
                    cpu,
                    previous_ms,
                    ctx.now_ms,
                    self.primed,
                );
            }
            part.cores.push(core);
        }
        if self.primed {
            part.thr_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"THR",
                self.irq.cpu_count,
                previous_ms,
                ctx.now_ms,
            );
            part.mce_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"MCE",
                self.irq.cpu_count,
                previous_ms,
                ctx.now_ms,
            );
            part.dfr_per_s = named_total_rate(
                &self.irq,
                &self.previous_irq,
                b"DFR",
                self.irq.cpu_count,
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
                let mut current_total = 0_u64;
                let mut previous_total = 0_u64;
                let mut valid = true;
                for cpu in 0..self.irq.cpu_count {
                    current_total = current_total.saturating_add(row.counts[cpu]);
                    previous_total = previous_total.saturating_add(previous.counts[cpu]);
                    valid &= row.counts[cpu].checked_sub(previous.counts[cpu]).is_some();
                }
                if valid
                    && let Some(per_s) =
                        counter_rate(current_total, previous_total, previous_ms, ctx.now_ms)
                {
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
        self.irq = InterruptsTable::with_capacity();
        self.previous_irq = InterruptsTable::with_capacity();
    }
}
fn counter_rate(current: u64, previous: u64, previous_ms: Option<u64>, now_ms: u64) -> Option<f64> {
    let mut counter = Counter::new();
    match counter.rate(previous, previous_ms?) {
        None => {}
        Some(_) => return None,
    }
    counter.rate(current, now_ms)
}
fn named_cell_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    name: &[u8],
    cpu: usize,
    previous_ms: Option<u64>,
    now_ms: u64,
    primed: bool,
) -> Option<f64> {
    if !primed {
        return None;
    }
    let hash = interrupts::hash_name(name);
    let current_row = current.named.iter().find(|row| row.hash == hash)?;
    let old_row = previous.named.iter().find(|row| row.hash == hash)?;
    counter_rate(
        current_row.counts[cpu],
        old_row.counts[cpu],
        previous_ms,
        now_ms,
    )
}
fn named_total_rate(
    current: &InterruptsTable,
    previous: &InterruptsTable,
    name: &[u8],
    cpus: usize,
    previous_ms: Option<u64>,
    now_ms: u64,
) -> Option<f64> {
    let hash = interrupts::hash_name(name);
    let current_row = current.named.iter().find(|row| row.hash == hash)?;
    let old_row = previous.named.iter().find(|row| row.hash == hash)?;
    let mut current_total = 0_u64;
    let mut previous_total = 0_u64;
    for cpu in 0..cpus {
        current_total = current_total.checked_add(current_row.counts[cpu])?;
        previous_total = previous_total.checked_add(old_row.counts[cpu])?;
        if current_row.counts[cpu]
            .checked_sub(old_row.counts[cpu])
            .is_none()
        {
            return None;
        }
    }
    counter_rate(current_total, previous_total, previous_ms, now_ms)
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
            .position(|top| top.name_hash == interrupts::hash_name(b"TIMER"))
            .unwrap();
        assert_eq!(frame.parts.cpu_irq.cores[0].softirq_per_s[timer_slot], None);
    }
}
