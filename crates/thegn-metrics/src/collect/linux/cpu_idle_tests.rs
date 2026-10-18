use super::*;

fn state_tree(root: &Path) -> PathBuf {
    let cpu = root.join("devices/system/cpu");
    std::fs::create_dir_all(cpu.join("cpu0/cpuidle/state0")).unwrap();
    std::fs::write(cpu.join("online"), "0\n").unwrap();
    for (file, value) in [
        ("name", "C1\n"),
        ("latency", "2\n"),
        ("disable", "0\n"),
        ("time", "100000\n"),
    ] {
        std::fs::write(cpu.join(format!("cpu0/cpuidle/state0/{file}")), value).unwrap();
    }
    std::fs::create_dir_all(cpu.join("cpu0/cpuidle/state1")).unwrap();
    for (file, value) in [
        ("name", "C2\n"),
        ("latency", "20\n"),
        ("disable", "0\n"),
        ("time", "200000\n"),
    ] {
        std::fs::write(cpu.join(format!("cpu0/cpuidle/state1/{file}")), value).unwrap();
    }
    cpu
}

#[test]
fn held_state_times_produce_residency_and_c0_from_two_samples() {
    let root = tempfile::tempdir().unwrap();
    let cpu = state_tree(root.path());
    let mut collector = CpuIdleCollector::with_root(root.path());
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    assert_eq!(collector.cadence(), Cadence::Every(5));
    assert_eq!(collector.state_count(), 2);
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 5,
                now_ms: 1_000,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(frame.parts.cpu_idle.cores[0].states[0].residency_pct, None);
    std::fs::write(cpu.join("cpu0/cpuidle/state0/time"), "250000\n").unwrap();
    std::fs::write(cpu.join("cpu0/cpuidle/state1/time"), "350000\n").unwrap();
    collector
        .sample(
            &TickCtx {
                tick: 10,
                now_ms: 6_000,
            },
            &mut frame,
        )
        .unwrap();
    let core = &frame.parts.cpu_idle.cores[0];
    assert!((core.states[0].residency_pct.unwrap() - 3.0).abs() < 0.01);
    assert!((core.states[1].residency_pct.unwrap() - 3.0).abs() < 0.01);
    assert!((core.c0_pct.unwrap() - 94.0).abs() < 0.01);
}

#[test]
fn backwards_time_degrades_only_that_attribute_and_missing_cpuidle_is_absent() {
    let root = tempfile::tempdir().unwrap();
    let cpu = state_tree(root.path());
    let mut collector = CpuIdleCollector::with_root(root.path());
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 1_000,
            },
            &mut frame,
        )
        .unwrap();
    std::fs::write(cpu.join("cpu0/cpuidle/state0/time"), "10\n").unwrap();
    std::fs::write(cpu.join("cpu0/cpuidle/state1/time"), "250000\n").unwrap();
    collector
        .sample(
            &TickCtx {
                tick: 6,
                now_ms: 6_000,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(frame.parts.cpu_idle.cores[0].states[0].residency_pct, None);
    assert!(
        frame.parts.cpu_idle.cores[0].states[1]
            .residency_pct
            .is_some()
    );

    let missing = tempfile::tempdir().unwrap();
    let missing_cpu = missing.path().join("devices/system/cpu");
    std::fs::create_dir_all(&missing_cpu).unwrap();
    std::fs::write(missing_cpu.join("online"), "0\n").unwrap();
    let mut absent = CpuIdleCollector::with_root(missing.path());
    assert_eq!(
        absent.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}
