//! Visible-only per-process GPU accounting from Linux DRM fdinfo.

use std::path::PathBuf;

use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::gpu_clients::GpuClientRow,
    parse::fdinfo::{self, Baseline},
    schedule::Cadence,
};

use super::drm_fdinfo::{DrmFdinfoScanner, FdFs, RealFdFs};

const CLIENT_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct ClientBaseline {
    pid: u32,
    start_time: u64,
    client_id: u64,
    pdev: [u8; 32],
    pdev_len: u8,
    baseline: Option<Baseline>,
}

pub struct GpuClientsCollector<F: FdFs = RealFdFs> {
    scanner: Option<DrmFdinfoScanner<F>>,
    drm_root: PathBuf,
    discovered: bool,
    fdinfo: Vec<u8>,
    baselines: Vec<ClientBaseline>,
    rows: Vec<GpuClientRow>,
    truncated: bool,
}

impl GpuClientsCollector<RealFdFs> {
    pub fn new() -> Self {
        Self {
            scanner: None,
            drm_root: PathBuf::from("/dev/dri"),
            discovered: false,
            fdinfo: Vec::new(),
            baselines: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        }
    }
}

impl Default for GpuClientsCollector<RealFdFs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FdFs> GpuClientsCollector<F> {
    pub fn at(fs: F, drm_root: impl Into<PathBuf>) -> Self {
        Self {
            scanner: Some(DrmFdinfoScanner::new(fs)),
            drm_root: drm_root.into(),
            discovered: false,
            fdinfo: Vec::with_capacity(4096),
            baselines: Vec::with_capacity(CLIENT_CAPACITY),
            rows: Vec::with_capacity(CLIENT_CAPACITY),
            truncated: false,
        }
    }
}

impl<F: FdFs + Send> Collector for GpuClientsCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::GpuClients
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        self.discovered = true;
        if !self.drm_root.is_dir() {
            return Health::Absent(Absent::NoHardware);
        }
        if self.scanner.is_none() {
            match F::open_default() {
                Ok(fs) => self.scanner = Some(DrmFdinfoScanner::new(fs)),
                Err(_) => return Health::Absent(Absent::NoPermission),
            }
        }
        Health::Warming
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if !self.discovered {
            return Err(CollectError::Gone);
        }
        if self.baselines.capacity() == 0 {
            self.baselines.reserve(CLIENT_CAPACITY);
            self.rows.reserve(CLIENT_CAPACITY);
            self.fdinfo.reserve(4096);
        }
        self.scanner
            .as_mut()
            .expect("discovery initializes scanner")
            .tick();
        self.truncated = false;
        self.rows.clear();
        let client_count = self
            .scanner
            .as_ref()
            .expect("discovery initializes scanner")
            .clients()
            .len();
        for index in 0..client_count {
            let client = self
                .scanner
                .as_ref()
                .expect("discovery initializes scanner")
                .clients()[index];
            if index >= CLIENT_CAPACITY {
                self.truncated = true;
                break;
            }
            self.fdinfo.clear();
            if self
                .scanner
                .as_mut()
                .expect("discovery initializes scanner")
                .fs_mut()
                .read_fdinfo(client.pid, client.fd, &mut self.fdinfo)
                .is_err()
            {
                continue;
            }
            let parsed = fdinfo::parse(&self.fdinfo);
            let baseline_index = self.baselines.iter().position(|old| {
                old.pid == client.pid
                    && old.start_time == client.start_time
                    && old.client_id == client.client_id
                    && old.pdev_len == client.pdev_len
                    && old.pdev[..usize::from(old.pdev_len)]
                        == client.pdev[..usize::from(client.pdev_len)]
            });
            let baseline = if let Some(index) = baseline_index {
                &mut self.baselines[index].baseline
            } else {
                if self.baselines.len() == self.baselines.capacity() {
                    self.truncated = true;
                    continue;
                }
                self.baselines.push(ClientBaseline {
                    pid: client.pid,
                    start_time: client.start_time,
                    client_id: client.client_id,
                    pdev: client.pdev,
                    pdev_len: client.pdev_len,
                    baseline: None,
                });
                &mut self
                    .baselines
                    .last_mut()
                    .expect("baseline was inserted")
                    .baseline
            };
            let busy = fdinfo::delta(
                baseline,
                client.start_time,
                ctx.now_ms.saturating_mul(1_000_000),
                &parsed,
            )
            .map(|delta| {
                std::array::from_fn(|index| {
                    (delta.valid & (1 << index) != 0).then_some(delta.util[index] * 100.0)
                })
            })
            .unwrap_or([None; fdinfo::ENGINE_SLOTS]);
            self.rows.push(GpuClientRow {
                pid: client.pid,
                start_time: client.start_time,
                pdev: client.pdev,
                pdev_len: client.pdev_len,
                client_id: client.client_id,
                engine_busy_pct: busy,
                memory_resident: memory_resident(&self.fdinfo),
                memory_total: memory_total(&self.fdinfo),
            });
        }
        self.baselines.retain(|baseline| {
            self.scanner
                .as_ref()
                .expect("discovery initializes scanner")
                .clients()
                .iter()
                .any(|client| {
                    client.pid == baseline.pid
                        && client.start_time == baseline.start_time
                        && client.client_id == baseline.client_id
                        && client.pdev_len == baseline.pdev_len
                        && client.pdev[..usize::from(client.pdev_len)]
                            == baseline.pdev[..usize::from(baseline.pdev_len)]
                })
        });
        let output = &mut frame.parts.gpu_clients;
        output.clients.clear();
        output.clients.extend(self.rows.iter().cloned());
        output.truncated = self.truncated;
        output.pids_truncated = self
            .scanner
            .as_ref()
            .expect("discovery initializes scanner")
            .truncated;
        frame.sampled.insert(CollectorId::GpuClients);
        Ok(())
    }

    fn close(&mut self) {
        self.discovered = false;
        self.baselines.clear();
        self.rows.clear();
        if let Some(scanner) = self.scanner.as_mut() {
            scanner.clear();
        }
    }
}

fn memory_resident(bytes: &[u8]) -> Option<u64> {
    const RESIDENT: &[&[u8]] = &[
        b"drm-resident-vram",
        b"drm-resident-gtt",
        b"drm-resident-local0",
        b"drm-resident-system0",
    ];
    if crate::collect::parse::kv_lines(bytes).any(|(key, _)| RESIDENT.contains(&key)) {
        memory_sum(bytes, RESIDENT)
    } else {
        memory_sum(
            bytes,
            &[
                b"drm-memory-vram",
                b"drm-memory-gtt",
                b"drm-memory-local0",
                b"drm-memory-system0",
            ],
        )
    }
}

fn memory_total(bytes: &[u8]) -> Option<u64> {
    memory_sum(
        bytes,
        &[
            b"drm-total-vram",
            b"drm-total-gtt",
            b"drm-total-local0",
            b"drm-total-system0",
        ],
    )
}

fn memory_sum(bytes: &[u8], keys: &[&[u8]]) -> Option<u64> {
    let mut total = 0_u64;
    let mut found = false;
    for (key, value) in crate::collect::parse::kv_lines(bytes) {
        if keys.contains(&key) {
            let mut fields = value
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty());
            let amount = fields.next().and_then(crate::collect::parse::parse_u64);
            let unit = fields.next();
            if let Some(amount) = amount {
                let multiplier = match unit {
                    None => 1,
                    Some(unit) if unit == b"KiB" => 1024,
                    Some(unit) if unit == b"MiB" => 1024 * 1024,
                    Some(_) => continue,
                };
                if let Some(value) = amount.checked_mul(multiplier)
                    && let Some(sum) = total.checked_add(value)
                {
                    total = sum;
                    found = true;
                }
            }
        }
    }
    found.then_some(total)
}

#[cfg(test)]
#[path = "gpu_clients_tests.rs"]
mod tests;
