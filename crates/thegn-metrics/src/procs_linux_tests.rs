use super::ProcTable;
use crate::procs_fs::{FakeProcFs, FakeProcess};
use std::time::Duration;

fn process(pid: u32, starttime: u64, utime: u64) -> FakeProcess {
    let stat =
        format!("{pid} (p{pid}) S 1 1 1 0 0 0 0 0 0 0 {utime} 0 0 0 0 0 1 0 {starttime} 1 1");
    FakeProcess {
        stat: stat.into_bytes(),
        uid: pid + 1000,
        cmdline: format!("p{pid}\0--test\0").into_bytes(),
        cgroup: b"0::/user.slice/test.scope\n".to_vec(),
    }
}

fn table_with(count: u32, kept_max: usize) -> ProcTable<FakeProcFs> {
    let mut fs = FakeProcFs::new();
    for pid in 1..=count {
        fs.insert(pid, process(pid, 10, 3));
    }
    ProcTable::with_kept_max(fs, kept_max)
}

#[test]
fn steady_pass_under_budget_and_does_not_grow_entry_table() {
    let mut table = table_with(722, 4096);
    let visible = (1..=40).collect::<Vec<_>>();
    table.pass(Duration::from_secs(1), &visible).unwrap();
    let len = table.entries().len();
    let capacity = table.entries.capacity();
    table.fs_mut().reset_ops();

    table.pass(Duration::from_secs(2), &visible).unwrap();

    assert_eq!(table.entries().len(), len);
    assert_eq!(table.entries.capacity(), capacity);
    assert!(table.fs().ops().total() <= 1000);
    assert_eq!(table.fs().ops().lists, 1);
    assert_eq!(table.fs().ops().reads, 722);
}

#[test]
fn pid_reuse_changes_identity_without_cpu_delta() {
    let mut table = table_with(1, 8);
    table.pass(Duration::from_secs(1), &[1]).unwrap();
    table.fs_mut().insert(1, process(1, 99, 0));

    table.pass(Duration::from_secs(2), &[1]).unwrap();

    assert_eq!(table.entries()[0].id.start_ticks, 99);
    assert!(table.entries()[0].cpu_delta.is_none());
}

#[test]
fn same_identity_cpu_counters_use_saturating_deltas_over_elapsed_time() {
    let mut table = table_with(1, 8);
    table.pass(Duration::from_secs(1), &[1]).unwrap();
    table.fs_mut().insert(1, process(1, 10, 8));
    table.pass(Duration::from_secs(3), &[1]).unwrap();
    assert_eq!(table.entries()[0].cpu_delta.unwrap().ticks, 5);

    table.fs_mut().insert(1, process(1, 10, 2));
    table.pass(Duration::from_secs(4), &[1]).unwrap();

    let delta = table.entries()[0].cpu_delta.unwrap();
    assert_eq!(delta.ticks, 0);
    assert_eq!(delta.elapsed, Duration::from_secs(1));
}

#[test]
fn exited_stat_read_becomes_ghost_and_closes_handle() {
    let mut table = table_with(1, 8);
    table.pass(Duration::from_secs(1), &[1]).unwrap();
    table.fs_mut().exit_on_next_read(1);

    table.pass(Duration::from_secs(2), &[1]).unwrap();

    assert!(table.entries()[0].ghost);
    assert!(!table.entries()[0].has_kept_fd());
    assert_eq!(table.fs().ops().closes, 1);
}

#[test]
fn over_budget_falls_back_and_closes_least_recently_visible_entry() {
    let mut table = table_with(8, 8);
    table.pass(Duration::from_secs(1), &[1]).unwrap();
    table.fs_mut().insert(9, process(9, 10, 3));
    table.fs_mut().reset_ops();

    table.pass(Duration::from_secs(2), &[9]).unwrap();

    let victim = table
        .entries()
        .iter()
        .find(|entry| entry.id.pid == 2)
        .unwrap();
    let fallback = table
        .entries()
        .iter()
        .find(|entry| entry.id.pid == 9)
        .unwrap();
    assert!(!victim.has_kept_fd());
    assert!(!fallback.has_kept_fd());
    assert_eq!(table.fs().ops().opens, 1);
    assert_eq!(table.fs().ops().closes, 2);
}

#[test]
fn reset_releases_every_kept_descriptor() {
    let mut table = table_with(8, 8);
    table.pass(Duration::from_secs(1), &[]).unwrap();

    table.reset();

    assert!(table.entries().is_empty());
    assert_eq!(table.fs().ops().opens, table.fs().ops().closes);
}

#[test]
fn unsorted_directory_listing_is_sorted_before_merge_join() {
    let mut fs = FakeProcFs::new();
    for pid in [1, 2, 3] {
        fs.insert(pid, process(pid, 10, 3));
    }
    fs.set_list_order(vec![3, 1, 2]);
    let mut table = ProcTable::with_kept_max(fs, 8);

    table.pass(Duration::from_secs(1), &[]).unwrap();

    assert_eq!(
        table
            .entries()
            .iter()
            .map(|entry| entry.id.pid)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}
