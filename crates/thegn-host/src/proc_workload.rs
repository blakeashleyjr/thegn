//! Opt-in real sampler-loop and monitor renderer measurements. This fixture
//! uses no terminal, DB, provider, user configuration or background recorder.
use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "proc_workload_adapter.rs"]
mod adapter;

#[derive(Clone, Copy)]
pub(crate) enum Event {
    ScheduledWait,
    ControlWait,
    SampleStart,
    SampleEnd,
    Reset,
    Publication,
    Wake,
}
#[derive(Default)]
pub(crate) struct Audit {
    counters: [AtomicU64; 7],
    sample_started: Mutex<Option<Instant>>,
    sample_ns: Mutex<Vec<u64>>,
    pub observed_gate: AtomicU8,
}
thread_local! { static ACTIVE: RefCell<Option<Arc<Audit>>> = const { RefCell::new(None) }; }
pub(crate) fn install(audit: Arc<Audit>) {
    ACTIVE.with(|slot| *slot.borrow_mut() = Some(audit));
}
pub(crate) fn event(event: Event) {
    ACTIVE.with(|slot| {
        if let Some(audit) = slot.borrow().as_ref() {
            audit.counters[event as usize].fetch_add(1, Ordering::Relaxed);
            match event {
                Event::SampleStart => *audit.sample_started.lock().unwrap() = Some(Instant::now()),
                Event::SampleEnd => {
                    let elapsed = audit
                        .sample_started
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .elapsed();
                    audit
                        .sample_ns
                        .lock()
                        .unwrap()
                        .push(elapsed.as_nanos() as u64);
                }
                _ => {}
            }
        }
    });
}
pub(crate) fn observed_gate(enabled: bool) {
    ACTIVE.with(|slot| {
        if let Some(audit) = slot.borrow().as_ref() {
            audit
                .observed_gate
                .store(if enabled { 2 } else { 1 }, Ordering::Release);
        }
    });
}
impl Audit {
    fn counts(&self) -> [u64; 7] {
        std::array::from_fn(|i| self.counters[i].load(Ordering::Relaxed))
    }
}
fn distribution(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    if values.is_empty() {
        return serde_json::json!({"n": 0});
    }
    serde_json::json!({"n": values.len(), "median_ns": values[values.len()/2], "p95_ns": values[(values.len()-1)*95/100], "max_ns": values[values.len()-1]})
}
fn process_context() -> serde_json::Value {
    // Linux-only contextual measurements; the per-worker event counters are the
    // portable scheduler evidence. These reads happen only at window boundaries.
    serde_json::json!({
        "self_stat": std::fs::read_to_string("/proc/self/stat").ok(),
        "self_status": std::fs::read_to_string("/proc/self/status").ok(),
        "os_process_count": std::fs::read_dir("/proc").ok().map(|entries| entries.filter_map(Result::ok).filter(|entry| entry.file_name().to_string_lossy().parse::<u32>().is_ok()).count()),
    })
}

#[test]
#[ignore = "owned real sampler/renderer performance workload; run alone in a release test binary"]
fn controlled_process_monitor_workload() {
    assert!(
        !cfg!(debug_assertions),
        "release dependencies are required for this workload"
    );
    // Event counts describe steady-state windows, not an atomic cross-thread
    // CPU/sample transaction. Initial prime/wake precedes the visible window;
    // a collection may straddle either boundary. Context reads are separate.
    let seconds: u64 = std::env::var("THEGN_AUDIT_PHASE_SECONDS")
        .unwrap_or_else(|_| "120".into())
        .parse()
        .unwrap();
    assert!(
        (120..=600).contains(&seconds),
        "acceptance windows must be between 120 and 600 seconds"
    );
    let audit = Arc::new(Audit::default());
    let mut driver = adapter::Driver::new(audit.clone());
    let mut windows = Vec::new();
    for (name, enabled) in [("hidden", false), ("visible", true), ("paused", false)] {
        driver.set_enabled(enabled);
        let admission_deadline = Instant::now() + Duration::from_secs(30);
        while !driver.acknowledged(enabled) {
            assert!(
                Instant::now() < admission_deadline,
                "worker did not acknowledge {name}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        while driver.take().is_some() {}
        let before = audit.counts();
        let sample_offset = audit.sample_ns.lock().unwrap().len();
        let context_before = process_context();
        let started = Instant::now();
        let mut consumed = 0u64;
        let mut consumer_timers = 0u64;
        while started.elapsed() < Duration::from_secs(seconds) {
            std::thread::sleep(Duration::from_millis(100));
            consumer_timers += 1;
            while let Some(snapshot) = driver.take() {
                std::hint::black_box(snapshot);
                consumed += 1;
            }
        }
        let context_after = process_context();
        let after = audit.counts();
        let delta: Vec<_> = after
            .into_iter()
            .zip(before)
            .map(|(after, before)| after - before)
            .collect();
        windows.push(serde_json::json!({
            "state": name, "elapsed_ms": started.elapsed().as_millis(),
            "scheduled_wait_returns": delta[Event::ScheduledWait as usize], "control_or_spurious_wait_returns": delta[Event::ControlWait as usize],
            "collections": delta[Event::SampleStart as usize], "completed_collections": delta[Event::SampleEnd as usize], "resets": delta[Event::Reset as usize], "publications": delta[Event::Publication as usize], "terminal_wake_attempts": delta[Event::Wake as usize],
            "consumed": consumed, "consumer_poll_timers": consumer_timers,
            "collection": distribution(audit.sample_ns.lock().unwrap()[sample_offset..].to_vec()),
            "context_before": context_before, "context_after": context_after,
        }));
    }
    driver.set_enabled(true);
    let reopened = Instant::now();
    let deadline = reopened + Duration::from_secs(30);
    let mut reopen = Vec::new();
    while reopen.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "worker did not resume two samples"
        );
        if let Some(snapshot) = driver.take() {
            reopen.push(serde_json::json!({"after_ms": reopened.elapsed().as_millis(), "primed": snapshot.primed, "total": snapshot.total}));
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(reopen[0]["primed"], false);
    assert_eq!(reopen[1]["primed"], true);
    driver.stop();
    let renderer = render_workloads();
    let result = serde_json::json!({
        "implementation": adapter::LABEL, "optimized_dependencies": true,
        "scope": "actual production sampler loop/collector and monitor refresh/render; excludes compositor, PTY and terminal I/O; consumer polling is measurement overhead",
        "counter_order": ["scheduled_wait", "control_wait", "sample_start", "sample_end", "reset", "publication", "wake"],
        "windows": windows, "reopen": reopen, "renderer": renderer,
    });
    use std::io::Write;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string_pretty(&result).unwrap()
    )
    .unwrap();
}

fn render_workloads() -> Vec<serde_json::Value> {
    use crate::chrome::FrameModel;
    use crate::compositor::Rect;
    use crate::detail::StatusCtx;
    use crate::monitor::{MonitorOverlay, MonitorPrefs, MonitorTab};
    use crate::telemetry::TelemetryHistory;
    use termwiz::surface::Surface;
    let mut output = Vec::new();
    for count in [64, 400] {
        let mut model = FrameModel::default();
        model.procs = thegn_metrics::ProcSnapshot {
            total: count,
            primed: true,
            enabled: true,
            procs: (0..count)
                .map(|i| thegn_metrics::ProcSample {
                    pid: 10_000 + i as u32,
                    ppid: None,
                    start_time: 1_000 + i as u64,
                    name: format!("fixture-{i:03}-世-e\u{301}-worker"),
                    cpu_pct: (i % 101) as f32,
                    rss_bytes: (i as u64 + 1) * 1_048_576,
                    run_secs: i as u64,
                    owner: thegn_metrics::ProcOwner::Other,
                })
                .collect(),
        };
        adapter::published(&mut model);
        for (cols, rows) in [(80, 24), (160, 48), (240, 72)] {
            let hist = TelemetryHistory::default();
            let screen = Rect::full(cols, rows);
            let ctx = StatusCtx::new_for_test_on(&hist, screen);
            let mut overlay =
                MonitorOverlay::open(MonitorTab::Procs, MonitorPrefs::default(), &model, &ctx);
            let mut surface = Surface::new(cols, rows);
            overlay.render(&mut surface, screen);
            let mut unchanged = Vec::with_capacity(500);
            let mut unchanged_dirty = 0;
            let mut unchanged_counts = Vec::with_capacity(500);
            for _ in 0..500 {
                let ((dirty, nanos), counts) = crate::proc_workload_alloc::measure(|| {
                    let start = Instant::now();
                    let dirty = overlay.refresh(std::hint::black_box(&model), &ctx);
                    (dirty, start.elapsed().as_nanos() as u64)
                });
                unchanged_dirty += dirty as usize;
                unchanged.push(nanos);
                unchanged_counts.push(counts);
            }
            let mut changed = Vec::with_capacity(50);
            let mut changed_counts = Vec::with_capacity(50);
            for tick in 0..50 {
                model.procs.procs[0].cpu_pct = tick as f32;
                model.procs.procs[0].rss_bytes += 1_048_576;
                adapter::published(&mut model);
                let (nanos, counts) = crate::proc_workload_alloc::measure(|| {
                    let start = Instant::now();
                    std::hint::black_box(overlay.refresh(&model, &ctx));
                    overlay.render(&mut surface, screen);
                    start.elapsed().as_nanos() as u64
                });
                changed.push(nanos);
                changed_counts.push(counts);
            }
            let rendered = surface
                .screen_cells()
                .iter()
                .map(|row| row.iter().map(|cell| cell.str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                rendered.contains("fixture-"),
                "actual process rows must be rendered"
            );
            output.push(serde_json::json!({"rows_in_snapshot": count, "cols": cols, "rows": rows, "unchanged_refresh": distribution(unchanged), "unchanged_reported_dirty": unchanged_dirty, "changed_refresh_and_render": distribution(changed), "unchanged_allocation_and_build_counts": count_totals(unchanged_counts), "changed_allocation_and_build_counts": count_totals(changed_counts)}));
        }
    }
    // Actual unrelated-tab refreshes: a process/disk row cache must not be
    // built just because an unrelated monitor tab receives its usual refresh.
    let model = FrameModel::default();
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let ctx = StatusCtx::new_for_test_on(&hist, screen);
    let mut overlay = MonitorOverlay::open(MonitorTab::Cpu, MonitorPrefs::default(), &model, &ctx);
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for _ in 0..500 {
            std::hint::black_box(overlay.refresh(&model, &ctx));
        }
    });
    output
        .push(serde_json::json!({"workload": "unrelated_cpu_tab_500_refreshes", "counts": counts}));
    output
}

fn count_totals(values: Vec<crate::proc_workload_alloc::Counts>) -> serde_json::Value {
    fn totals(values: impl Iterator<Item = u64>) -> serde_json::Value {
        let mut values: Vec<_> = values.collect();
        values.sort_unstable();
        serde_json::json!({"sum": values.iter().sum::<u64>(), "median": values[values.len()/2], "p95": values[(values.len()-1)*95/100], "max": values[values.len()-1]})
    }
    serde_json::json!({
        "samples": values.len(), "provenance": "calling thread only; allocation/reallocation calls and requested bytes, not live heap size",
        "allocation_calls": totals(values.iter().map(|v| v.allocation_calls)),
        "realloc_calls": totals(values.iter().map(|v| v.realloc_calls)),
        "deallocation_calls": totals(values.iter().map(|v| v.deallocation_calls)),
        "requested_bytes": totals(values.iter().map(|v| v.requested_bytes)),
        "process_row_builds": totals(values.iter().map(|v| v.process_row_builds)),
        "disk_row_builds": totals(values.iter().map(|v| v.disk_row_builds)),
        "body_builds": totals(values.iter().map(|v| v.body_builds)),
    })
}

#[test]
#[ignore = "paired release renderer/allocation workload; excludes sampler windows"]
fn controlled_process_monitor_render_workload() {
    assert!(!cfg!(debug_assertions), "release dependencies are required");
    use std::io::Write;
    writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&serde_json::json!({
        "implementation": adapter::LABEL,
        "scope": "actual monitor refresh/render, with paired test-only TLS System allocator forwarding overhead; no compositor/PTY/terminal",
        "renderer": render_workloads(),
    })).unwrap()).unwrap();
}
