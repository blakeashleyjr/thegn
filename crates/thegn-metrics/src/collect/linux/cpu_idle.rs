//! Linux cpuidle state residency using held time-file descriptors.

use std::{
    fs::File,
    io::Read,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::cpu_idle::{CpuIdleCore, CpuIdleState},
    schedule::Cadence,
};

struct StateSlot {
    state: CpuIdleState,
    time: Option<File>,
    previous_us: Option<u64>,
}
struct CoreSlot {
    cpu: u16,
    states: Vec<StateSlot>,
}

pub struct CpuIdleCollector {
    root: PathBuf,
    cores: Vec<CoreSlot>,
    previous_ms: Option<u64>,
}
impl CpuIdleCollector {
    pub fn new() -> Self {
        Self::with_root("/sys")
    }
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cores: Vec::new(),
            previous_ms: None,
        }
    }
    fn text(path: &Path) -> Option<String> {
        let mut file = File::open(path).ok()?;
        let mut value = String::new();
        file.read_to_string(&mut value).ok()?;
        Some(value.trim().to_owned())
    }
    #[cfg(test)]
    pub fn state_count(&self) -> usize {
        self.cores.iter().map(|core| core.states.len()).sum()
    }
}
impl Default for CpuIdleCollector {
    fn default() -> Self {
        Self::new()
    }
}
fn read_time(file: Option<&File>) -> Option<u64> {
    let mut bytes = [0_u8; 32];
    let count = file?.read_at(&mut bytes, 0).ok()?;
    std::str::from_utf8(&bytes[..count])
        .ok()?
        .trim()
        .parse()
        .ok()
}
impl Collector for CpuIdleCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CpuIdle
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.cores.clear();
        self.previous_ms = None;
        let cpu_root = self.root.join("devices/system/cpu");
        let online = Self::text(&cpu_root.join("online"))
            .and_then(|value| super::super::policy::cpu_topology::parse_cpulist(&value));
        let Some(online) = online else {
            return Health::Absent(Absent::NoHardware);
        };
        for cpu in online {
            let base = cpu_root.join(format!("cpu{cpu}/cpuidle"));
            if !base.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&base) else {
                continue;
            };
            let mut states = Vec::new();
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|part| part.to_str()) else {
                    continue;
                };
                let Some(index) = name
                    .strip_prefix("state")
                    .and_then(|value| value.parse::<u8>().ok())
                else {
                    continue;
                };
                let state_name = Self::text(&path.join("name"));
                let latency_us =
                    Self::text(&path.join("latency")).and_then(|value| value.parse().ok());
                let disabled =
                    Self::text(&path.join("disable")).and_then(|value| match value.as_str() {
                        "0" => Some(false),
                        "1" => Some(true),
                        _ => None,
                    });
                let time = match File::open(path.join("time")) {
                    Ok(file) => Some(file),
                    Err(_) => None,
                };
                states.push(StateSlot {
                    state: CpuIdleState {
                        index,
                        name: state_name,
                        latency_us,
                        disabled,
                        residency_pct: None,
                    },
                    time,
                    previous_us: None,
                });
            }
            states.sort_by_key(|slot| slot.state.index);
            self.cores.push(CoreSlot { cpu, states });
        }
        if self.cores.is_empty() {
            return Health::Absent(Absent::NoHardware);
        }
        Health::Warming
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let elapsed_ms = self
            .previous_ms
            .map(|previous| ctx.now_ms.saturating_sub(previous));
        let output = &mut frame.parts.cpu_idle;
        while output.cores.len() < self.cores.len() {
            output.cores.push(CpuIdleCore::default());
        }
        output.cores.truncate(self.cores.len());
        for (core, published) in self.cores.iter_mut().zip(&mut output.cores) {
            let mut sum = 0.0_f32;
            let mut valid = false;
            while published.states.len() < core.states.len() {
                published.states.push(CpuIdleState::default());
            }
            published.states.truncate(core.states.len());
            for (state, slot) in published.states.iter_mut().zip(&mut core.states) {
                let current = read_time(slot.time.as_ref());
                slot.state.residency_pct = match (slot.previous_us, current, elapsed_ms) {
                    (Some(previous), Some(now), Some(ms)) if now >= previous && ms > 0 => {
                        valid = true;
                        let pct = (now - previous) as f32 / (ms as f32 * 10.0);
                        let bounded = pct.clamp(0.0, 100.0);
                        sum += bounded;
                        Some(bounded)
                    }
                    _ => None,
                };
                slot.previous_us = current;
                state.clone_from(&slot.state);
            }
            published.cpu = core.cpu;
            published.c0_pct = valid.then_some((100.0 - sum).clamp(0.0, 100.0));
        }
        self.previous_ms = Some(ctx.now_ms);
        frame.sampled.insert(CollectorId::CpuIdle);
        Ok(())
    }
    fn close(&mut self) {
        self.cores.clear();
        self.previous_ms = None;
    }
}

#[cfg(test)]
#[path = "cpu_idle_tests.rs"]
mod tests;
