use std::time::{Duration, Instant};

use crate::collect::ProcSampleRequest;
use crate::procs_fs::{FakeProcFs, FakeProcess};
use crate::procs_linux::ProcTable;

use super::{NativeBackend, owner_of_table, reset_with, sample_with};

fn process(pid: u32, ppid: u32, utime: u64, starttime: u64) -> FakeProcess {
    let stat = format!(
        "{pid} (worker {pid}) S {ppid} 1 1 0 0 0 0 0 0 0 {utime} 0 0 0 0 0 1 0 {starttime} 1 1"
    );
    FakeProcess {
        stat: stat.into_bytes(),
        uid: 1000,
        cmdline: vec![b'w'; 4096],
        cgroup: Vec::new(),
    }
}

fn request(pane_pids: &[(u32, u32)], rows: usize) -> ProcSampleRequest<'_> {
    ProcSampleRequest {
        rows,
        self_pid: 99,
        daemon_pid: None,
        pane_pids,
        primed: false,
    }
}

#[test]
fn sampler_backend_samples_and_reset_clear_native_baseline_and_kept_handles() {
    let mut fs = FakeProcFs::new();
    for pid in 1..=3 {
        fs.insert(pid, process(pid, 1, 10, 250));
    }
    let mut native = NativeBackend::with(fs, 8, Some(1_700_000_000), Some(100), Some(4096));
    let panes = [(1, 4)];
    let mut primed = false;

    let (first, successful) = sample_with(
        &mut native,
        request(&panes, 20),
        Instant::now(),
        20,
        &mut primed,
    );
    assert!(successful);
    assert!(!first.primed);
    primed = successful;
    assert_eq!(first.procs.len(), 3);
    assert!(first.procs.iter().all(|row| row.start_ticks == Some(250)));
    assert!(native.last_sample.is_some());
    assert_eq!(native.pass_time, Duration::ZERO);

    native.table.fs_mut().reset_ops();
    let (second, successful) = sample_with(
        &mut native,
        request(&panes, 20),
        Instant::now() + Duration::from_secs(2),
        22,
        &mut primed,
    );
    assert!(successful);
    assert!(second.primed);
    assert_eq!(
        native.table.fs().ops().reads,
        3,
        "one stat read per listed pid"
    );
    primed = successful;
    assert!(primed);
    // Both instants come from separate Instant::now() calls, so allow scheduling jitter.
    assert!(native.pass_time >= Duration::from_secs(2) && native.pass_time < Duration::from_secs(3));
    assert!(!native.previous_pids.is_empty());

    let held_before_reset = native.table.kept_count();
    native.table.fs_mut().reset_ops();
    reset_with(&mut native, &mut primed);
    assert_eq!(native.table.entries().len(), 0);
    assert_eq!(native.table.kept_count(), 0);
    assert_eq!(native.table.fs().ops().closes, held_before_reset);
    assert!(
        native
            .table
            .entries()
            .iter()
            .all(|entry| !entry.has_kept_fd())
    );
    assert!(native.previous_pids.is_empty());
    assert_eq!(native.pass_time, Duration::ZERO);
    assert!(native.last_sample.is_none());
    assert!(!primed);

    let (after_reset, successful) = sample_with(
        &mut native,
        request(&panes, 20),
        Instant::now() + Duration::from_secs(10),
        30,
        &mut primed,
    );
    assert!(successful);
    assert!(!after_reset.primed);
}

#[test]
fn native_projection_uses_pass_stat_and_keeps_the_722_pid_pass_bounded() {
    let mut fs = FakeProcFs::new();
    for pid in 1..=722_u32 {
        let parent = if pid == 1 { 1 } else { 1000 };
        fs.insert(pid, process(pid, parent, 10, 250));
    }
    let mut native = NativeBackend::with(fs, 4096, Some(1_700_000_000), Some(100), Some(4096));
    let visible = (1..=40).map(|pid| (pid, 7)).collect::<Vec<_>>();
    let mut primed = false;

    let (first, successful) = sample_with(
        &mut native,
        request(&visible, 40),
        Instant::now(),
        20,
        &mut primed,
    );
    assert!(successful);
    assert!(!first.primed);
    native.table.fs_mut().reset_ops();

    let (snapshot, successful) = sample_with(
        &mut native,
        request(&visible, 40),
        Instant::now() + Duration::from_secs(2),
        20,
        &mut primed,
    );

    assert!(successful);
    assert!(native.table.fs().ops().total() <= 1000);
    assert_eq!(
        native.table.fs().ops().reads,
        722,
        "one stat read per listed PID"
    );
    assert_eq!(snapshot.total, 722);
    assert_eq!(snapshot.procs.len(), 40);
    let first = snapshot.procs.iter().find(|row| row.pid == 1).unwrap();
    assert_eq!(first.ppid, Some(1));
    assert_eq!(first.name, "worker 1");
    assert_eq!(first.start_ticks, Some(250));
    assert_eq!(first.start_time, 1_700_000_002);
    assert_eq!(first.rss_bytes, 4096);
    assert_eq!(first.owner, crate::procs::ProcOwner::Pane(7));

    native.boot_time = None;
    let (without_btime, successful) = sample_with(
        &mut native,
        request(&visible, 40),
        Instant::now() + Duration::from_secs(4),
        20,
        &mut primed,
    );
    assert!(successful);
    let first_without_btime = without_btime.procs.iter().find(|row| row.pid == 1).unwrap();
    assert_eq!(first_without_btime.start_time, 0);
    assert_eq!(first_without_btime.start_ticks, Some(250));
}

#[test]
fn native_owner_walk_matches_self_daemon_and_pane_rules() {
    let mut fs = FakeProcFs::new();
    for (pid, parent) in [(10, 20), (20, 30), (30, 40), (40, 40)] {
        fs.insert(pid, process(pid, parent, 10, 250));
    }
    let mut table = ProcTable::with_kept_max(fs, 0);
    table.pass(Duration::from_secs(1), &[]).unwrap();
    let panes = [(30, 8)];

    assert_eq!(
        owner_of_table(&table, 10, 40, None, &[]),
        crate::procs::ProcOwner::ThegnSelf
    );
    assert_eq!(
        owner_of_table(&table, 10, 99, Some(40), &[]),
        crate::procs::ProcOwner::ThegnDaemon
    );
    assert_eq!(
        owner_of_table(&table, 10, 99, None, &panes),
        crate::procs::ProcOwner::Pane(8)
    );
    assert_eq!(
        owner_of_table(&table, 10, 99, None, &[]),
        crate::procs::ProcOwner::Other
    );
}

#[test]
fn failed_list_does_not_mark_a_native_sampler_primed() {
    let mut fs = FakeProcFs::new();
    fs.insert(7, process(7, 1, 10, 250));
    let mut native = NativeBackend::with(fs, 8, None, Some(100), Some(4096));
    native
        .table
        .fs_mut()
        .fail_next_list(std::io::ErrorKind::PermissionDenied);

    let mut primed = false;
    let (failed, successful) = sample_with(
        &mut native,
        request(&[], 20),
        Instant::now(),
        10,
        &mut primed,
    );
    assert!(!successful);
    assert!(!failed.primed);
    assert!(!primed);
    assert!(native.previous_pids.is_empty());

    let (recovered, successful) = sample_with(
        &mut native,
        request(&[], 20),
        Instant::now() + Duration::from_secs(2),
        12,
        &mut primed,
    );
    assert!(successful);
    assert!(!recovered.primed);
}

#[test]
fn native_projection_omits_zero_parent_pid() {
    let mut fs = FakeProcFs::new();
    fs.insert(1, process(1, 0, 10, 250));
    let mut native = NativeBackend::with(fs, 8, None, Some(100), Some(4096));
    let mut primed = false;
    let (snapshot, successful) = sample_with(
        &mut native,
        request(&[], 20),
        Instant::now(),
        10,
        &mut primed,
    );

    assert!(successful);
    assert_eq!(snapshot.procs.len(), 1);
    assert_eq!(snapshot.procs[0].ppid, None);
}
