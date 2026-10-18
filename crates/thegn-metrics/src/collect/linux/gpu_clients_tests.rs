use super::GpuClientsCollector;
use crate::collect::{
    Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    linux::drm_fdinfo::FdFs,
    registry::Registry,
    schedule::Mode,
};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeState {
    pids: Vec<u32>,
    start: u64,
    reads: usize,
    fdinfo_reads: BTreeMap<u32, usize>,
}

#[derive(Clone, Default)]
struct FakeFdFs(Arc<Mutex<FakeState>>);

impl FdFs for FakeFdFs {
    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reads += 1;
        out.clear();
        out.extend_from_slice(&state.pids);
        Ok(())
    }
    fn start_time(&mut self, _pid: u32) -> io::Result<u64> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reads += 1;
        Ok(state.start)
    }
    fn list_fds(&mut self, _pid: u32, out: &mut Vec<u32>) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads += 1;
        out.clear();
        out.push(3);
        Ok(())
    }
    fn readlink(&mut self, _pid: u32, _fd: u32, out: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads += 1;
        let target = b"/dev/dri/renderD128";
        out[..target.len()].copy_from_slice(target);
        Ok(target.len())
    }
    fn read_fdinfo(&mut self, pid: u32, _fd: u32, out: &mut Vec<u8>) -> io::Result<usize> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reads += 1;
        let reads = state.fdinfo_reads.entry(pid).or_default();
        *reads += 1;
        let amount = *reads as u64 * 10_000_000;
        out.clear();
        out.extend_from_slice(
            format!("drm-pdev:\t0000:03:00.0\ndrm-client-id:\t{}\ndrm-engine-gfx:\t{} ns\ndrm-resident-vram:\t4 KiB\n", pid, amount).as_bytes(),
        );
        Ok(out.len())
    }
}

#[test]
fn collector_publishes_two_clients_and_primes_then_reports_engine_deltas() {
    let root = tempfile::tempdir().unwrap();
    let dri = root.path().join("dri");
    std::fs::create_dir(&dri).unwrap();
    let fs = FakeFdFs::default();
    {
        let mut state =
            fs.0.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pids = vec![41, 42];
        state.start = 900;
    }
    let mut collector = GpuClientsCollector::at(fs, &dri);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 100,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(frame.parts.gpu_clients.clients.len(), 2);
    assert!(
        frame
            .parts
            .gpu_clients
            .clients
            .iter()
            .all(|row| row.engine_busy_pct[0].is_none())
    );
    collector
        .sample(
            &TickCtx {
                tick: 2,
                now_ms: 200,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(frame.parts.gpu_clients.clients.len(), 2);
    assert!(frame.parts.gpu_clients.clients.iter().all(|row| {
        row.engine_busy_pct[0].is_some_and(|percent| (percent - 10.0).abs() < 0.001)
            && row.memory_resident == Some(4096)
    }));
}

#[test]
fn pid_reuse_restarts_the_client_delta_baseline() {
    let root = tempfile::tempdir().unwrap();
    let dri = root.path().join("dri");
    std::fs::create_dir(&dri).unwrap();
    let fs = FakeFdFs::default();
    {
        let mut state =
            fs.0.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pids = vec![41];
        state.start = 900;
    }
    let fs_state = fs.0.clone();
    let mut collector = GpuClientsCollector::at(fs, &dri);
    collector.discover(&Env::default());
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 100,
            },
            &mut frame,
        )
        .unwrap();
    collector
        .sample(
            &TickCtx {
                tick: 2,
                now_ms: 200,
            },
            &mut frame,
        )
        .unwrap();
    fs_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .start = 901;
    collector
        .sample(
            &TickCtx {
                tick: 3,
                now_ms: 300,
            },
            &mut frame,
        )
        .unwrap();
    assert!(frame.parts.gpu_clients.clients[0].engine_busy_pct[0].is_none());
}

#[test]
fn missing_dri_root_is_absent_no_hardware() {
    let root = tempfile::tempdir().unwrap();
    let collector = GpuClientsCollector::at(FakeFdFs::default(), root.path().join("missing"));
    let mut boxed: Box<dyn Collector> = Box::new(collector);
    assert_eq!(
        boxed.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}

#[test]
fn registry_does_no_fd_operations_when_only_all_non_visible_ids_are_demanded() {
    let root = tempfile::tempdir().unwrap();
    let dri = root.path().join("dri");
    std::fs::create_dir(&dri).unwrap();
    let fs = FakeFdFs::default();
    fs.0.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pids = vec![41];
    let state = fs.0.clone();
    let mut registry = Registry::new(vec![Box::new(GpuClientsCollector::at(fs, &dri))]);
    registry.discover(&Env::default());
    let mut demand = CollectorSet::empty();
    for id in CollectorId::ALL {
        if !id.visible_only() {
            demand.insert(id);
        }
    }
    let mut frame = MonitorFrame::default();
    for tick in 0..8 {
        registry.tick(
            Mode::Open,
            demand,
            &TickCtx {
                tick,
                now_ms: tick * 100,
            },
            &mut frame,
        );
    }
    assert_eq!(
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads,
        0
    );
}
