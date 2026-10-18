use super::{DrmFdinfoScanner, FdFs, READLINKS_PER_TICK};
use std::io;

#[derive(Default)]
struct FakeFdFs {
    pids: Vec<u32>,
    descriptors: Vec<u32>,
    readlinks: usize,
    start_reads: usize,
    start: u64,
    overreport_lengths: bool,
}

impl FdFs for FakeFdFs {
    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        if self.pids.is_empty() {
            out.push(41);
        } else {
            out.extend_from_slice(&self.pids);
        }
        Ok(())
    }
    fn start_time(&mut self, _pid: u32) -> io::Result<u64> {
        self.start_reads += 1;
        Ok(self.start)
    }
    fn list_fds(&mut self, _pid: u32, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        out.extend_from_slice(&self.descriptors);
        Ok(())
    }
    fn readlink(&mut self, _pid: u32, fd: u32, out: &mut [u8]) -> io::Result<usize> {
        self.readlinks += 1;
        let target = if fd < 2 {
            b"/dev/dri/renderD128".as_slice()
        } else {
            b"/tmp/file".as_slice()
        };
        out[..target.len()].copy_from_slice(target);
        if self.overreport_lengths {
            Ok(out.len() + 1)
        } else {
            Ok(target.len())
        }
    }
    fn read_fdinfo(&mut self, _pid: u32, _fd: u32, out: &mut Vec<u8>) -> io::Result<usize> {
        out.clear();
        out.extend_from_slice(
            b"drm-pdev:\t0000:03:00.0\ndrm-client-id:\t7\ndrm-engine-gfx:\t20 ns\n",
        );
        if self.overreport_lengths {
            Ok(out.len() + 1)
        } else {
            Ok(out.len())
        }
    }
}

#[test]
fn duplicate_drm_fds_count_as_one_device_client_and_steady_state_keeps_capacity() {
    let fs = FakeFdFs {
        descriptors: vec![0, 1, 2, 3],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    assert_eq!(scanner.tick(), 4);
    assert_eq!(scanner.clients().len(), 1);
    assert_eq!(scanner.clients()[0].fd, 1);
    let capacity = (
        scanner.pids.capacity(),
        scanner.fd_scratch.capacity(),
        scanner.fdinfo.capacity(),
        scanner.clients.capacity(),
        scanner.processes[0].fds.capacity(),
    );
    for _ in 0..4 {
        assert!(scanner.tick() <= READLINKS_PER_TICK);
    }
    assert_eq!(
        capacity,
        (
            scanner.pids.capacity(),
            scanner.fd_scratch.capacity(),
            scanner.fdinfo.capacity(),
            scanner.clients.capacity(),
            scanner.processes[0].fds.capacity()
        )
    );
}

#[test]
fn changed_start_time_discards_old_client_identity_and_rescans() {
    let fs = FakeFdFs {
        descriptors: vec![0],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    scanner.tick();
    scanner.fs_mut().start = 901;
    scanner.tick();
    assert_eq!(scanner.clients().len(), 1);
    assert_eq!(scanner.clients()[0].start_time, 901);
}

#[test]
fn new_pid_every_tick_does_not_starve_the_rolling_sweep() {
    let fs = FakeFdFs {
        descriptors: vec![0],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    for pid in 41..46 {
        scanner.fs_mut().pids.push(pid);
        scanner.tick();
    }
    assert!(scanner.sweep_cursor > 0);
}

#[test]
fn same_device_client_in_two_pids_is_owned_once() {
    let fs = FakeFdFs {
        pids: vec![41, 42],
        descriptors: vec![0],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    scanner.tick();
    assert_eq!(scanner.clients().len(), 1);
    assert_eq!(scanner.clients()[0].pid, 41);
}

#[test]
fn rescan_prunes_clients_after_their_drm_fd_closes() {
    let fs = FakeFdFs {
        descriptors: vec![0],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    scanner.tick();
    assert_eq!(scanner.clients().len(), 1);
    scanner.fs_mut().descriptors.clear();
    scanner.tick();
    assert!(scanner.clients().is_empty());
}

#[test]
fn idle_processes_do_not_read_start_time_on_every_tick() {
    let fs = FakeFdFs {
        pids: vec![41, 42],
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    scanner.tick();
    let initial = scanner.fs().start_reads;
    for _ in 0..4 {
        scanner.tick();
    }
    assert!(scanner.fs().start_reads < initial + 8);
}

#[test]
fn oversized_backend_lengths_are_clamped_to_the_provided_buffers() {
    let fs = FakeFdFs {
        descriptors: vec![0],
        start: 900,
        overreport_lengths: true,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    assert!(scanner.tick() <= READLINKS_PER_TICK);
    assert_eq!(scanner.clients().len(), 1);
}

#[test]
fn pid_cap_overflow_is_reported() {
    let fs = FakeFdFs {
        pids: (1..=1025).collect(),
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    scanner.tick();
    assert!(scanner.truncated);
}

#[test]
fn seventeen_thousand_descriptors_respect_readlink_budget_each_tick() {
    let fs = FakeFdFs {
        descriptors: (0..17_000).collect(),
        start: 900,
        ..FakeFdFs::default()
    };
    let mut scanner = DrmFdinfoScanner::new(fs);
    for _ in 0..29 {
        assert!(scanner.tick() <= READLINKS_PER_TICK);
    }
    assert_eq!(scanner.fs().readlinks, 17_000);
}
