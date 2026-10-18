use super::{BaseCpu, usage};
use crate::collect::parse::stat::CpuTicks;
use std::fs;
use std::os::unix::fs::PermissionsExt;

const FIRST: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cpu/stat_24c"
));
const SECOND: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cpu/stat_vm_steal"
));

#[test]
fn fixture_deltas_match_sysinfo_busy_formula_for_global_and_each_core() {
    let mut reader = BaseCpu::with_path("unused");
    let mut primed = false;
    assert_eq!(reader.fill_bytes(&mut primed, FIRST), (None, Vec::new()));
    assert!(primed);

    let (global, cores) = reader.fill_bytes(&mut primed, SECOND);
    // Aggregate deltas: corrected user +480, steal +240, idle +1680, so
    // busy = 720 and total = 2400 (30%). Guest is subtracted from user and
    // added back as virtual time; guest is unchanged between these fixtures.
    // iowait is also unchanged, and when it changes sysinfo counts it as idle.
    assert_eq!(global, Some(30));
    // Each core has +20 user, +10 steal, and +70 idle: 30 / 100 = 30%.
    assert_eq!(cores, vec![30; 24]);
}

#[test]
fn backwards_counter_has_no_usage_instead_of_reporting_zero_or_full_load() {
    let previous = CpuTicks {
        user: 20,
        ..CpuTicks::default()
    };
    let current = CpuTicks {
        user: 10,
        ..CpuTicks::default()
    };
    assert_eq!(usage(previous, current), None);
}

#[test]
fn guest_ticks_count_as_busy_and_iowait_counts_as_idle() {
    let previous = CpuTicks {
        user: 100,
        nice: 20,
        system: 30,
        idle: 400,
        iowait: 20,
        irq: 2,
        softirq: 3,
        steal: 1,
        guest: 40,
        guest_nice: 5,
    };
    let current = CpuTicks {
        user: 120,
        nice: 30,
        system: 40,
        idle: 450,
        iowait: 30,
        irq: 4,
        softirq: 6,
        steal: 6,
        guest: 50,
        guest_nice: 10,
    };
    // Corrected user/nice deltas are 10/5, with 15 guest ticks added back as
    // virtual busy time. Busy = 50; total adds 50 idle + 10 iowait = 110.
    assert!(
        usage(previous, current)
            .is_some_and(|value| (value - (50.0 / 110.0 * 100.0)).abs() < 0.001)
    );
}

#[test]
fn missing_cpu_seven_keeps_cpu_eight_at_index_eight() {
    let mut reader = BaseCpu::with_path("unused");
    let mut first = b"cpu 100 0 0 100\n".to_vec();
    for id in 0..=8 {
        if id != 7 {
            first.extend_from_slice(format!("cpu{id} 100 0 0 100\n").as_bytes());
        }
    }
    let mut second = b"cpu 120 0 0 110\n".to_vec();
    for id in 0..=8 {
        if id != 7 {
            let user = if id == 8 { 120 } else { 110 };
            second.extend_from_slice(format!("cpu{id} {user} 0 0 110\n").as_bytes());
        }
    }
    let mut primed = false;
    assert_eq!(reader.fill_bytes(&mut primed, &first), (None, Vec::new()));
    let (_, cores) = reader.fill_bytes(&mut primed, &second);
    assert_eq!(cores.len(), 9);
    assert_eq!(cores[7], 0);
    assert_eq!(cores[8], 67);
}

#[test]
fn held_stat_file_is_read_once_per_fill_without_reopening() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("stat");
    fs::write(&path, FIRST).unwrap();
    let mut reader = BaseCpu::with_path(&path);
    let mut primed = false;
    reader.fill(&mut primed);
    assert_eq!(reader.read_count, 1);
    assert_eq!(reader.open_count, 1);

    fs::write(&path, SECOND).unwrap();
    reader.fill(&mut primed);
    assert_eq!(reader.read_count, 2);
    assert_eq!(reader.open_count, 1);
    assert!(reader.file.is_some());
}
