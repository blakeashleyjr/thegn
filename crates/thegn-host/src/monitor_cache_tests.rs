//! Actual monitor refresh/render invalidation and selection regressions.
use super::*;
use crate::proc_workload_alloc::measure;

#[test]
fn terminal_resize_reflows_frozen_process_body_without_consuming_model_updates() {
    let mut model = model_with_n_procs(30);
    for index in 1..30 {
        model.procs.procs[index].ppid = Some(model.procs.procs[index - 1].pid);
    }
    let frozen_model = model.clone();
    let hist = history(10, NOW_MS);
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    overlay.prefs.proc_tree = true;
    overlay.filter = "proc".into();
    overlay.rebuild_after_key(&model, &ctx_at(&hist, screen));
    overlay.nav(25);
    overlay.rebuild_after_key(&model, &ctx_at(&hist, screen));
    assert_eq!(ch(&mut overlay, ' '), MonitorOutcome::PrefsChanged);
    overlay.begin_signal();
    let frozen_rows = overlay.proc_rows.clone();
    let frozen_header = headings(&overlay);
    let frozen_body_key = overlay.process_body;
    assert!(overlay.process_rows_current(&frozen_model));
    let identity = (
        overlay.proc_rows[overlay.sel].pid,
        overlay.proc_rows[overlay.sel].start_time,
    );
    let confirmation = format!("{:?}", overlay.confirm);
    let prefs = overlay.prefs.clone();
    let clock = (
        overlay.last_now_ms,
        overlay.paused_at,
        overlay.frozen_now_ms,
        overlay.covered_secs,
    );

    // Hydration/sampling may change the current model underneath a paused view.
    // The actual resize seam cannot read it; even header capability/priming and
    // PID birth changes must wait for an ordinary unpaused data refresh.
    model.process_revision += 10;
    model.procs.procs[25].start_time += 1000;
    model.procs.procs[25].name = "new sample must remain hidden".into();
    model.procs.total += 100;
    model.procs.primed = false;
    model.procs_disabled = true;
    for (cols, rows) in [(80, 24), (240, 72), (160, 48)] {
        let next = Rect::full(cols, rows);
        assert!(!overlay.refresh(&model, &ctx_at(&hist, next)));
        let (changed, counts) = measure(|| overlay.reflow_geometry(next));
        assert!(changed);
        assert_eq!(
            (
                counts.process_row_builds,
                counts.disk_row_builds,
                counts.body_builds
            ),
            (0, 0, 1)
        );
        assert_eq!((overlay.cols, overlay.rows), MonitorOverlay::dims(next));
        assert_eq!(
            overlay.body_rows,
            overlay.rows.saturating_sub(super::super::CHROME_ROWS)
        );
        assert_eq!(overlay.proc_rows, frozen_rows);
        assert_eq!(headings(&overlay), frozen_header);
        assert_eq!(overlay.process_body, frozen_body_key);
        assert!(overlay.process_rows_current(&frozen_model));
        assert!(!overlay.process_rows_current(&model));
        assert_eq!(format!("{:?}", overlay.confirm), confirmation);
        assert_eq!(overlay.prefs, prefs);
        assert!(overlay.paused && overlay.follow);
        assert_eq!(
            (
                overlay.last_now_ms,
                overlay.paused_at,
                overlay.frozen_now_ms,
                overlay.covered_secs
            ),
            clock
        );
        assert_eq!(
            (
                overlay.proc_rows[overlay.sel].pid,
                overlay.proc_rows[overlay.sel].start_time
            ),
            identity
        );
        assert!(cursor_on_screen(&overlay));
        assert_eq!(overlay.filter, "proc");

        // Compare the actual reflowed table to the shipping builder at this
        // geometry using only the original snapshot, including deep tree indent.
        let mut expected = open_tab(MonitorTab::Procs, &frozen_model, &hist, next);
        expected.prefs = prefs.clone();
        expected.filter = "proc".into();
        expected.rebuild_after_key(&frozen_model, &ctx_at(&hist, next));
        let table = |ov: &MonitorOverlay| {
            ov.body
                .iter()
                .find_map(|section| match section {
                    Section::FixedTable { table, widths } => Some((
                        widths.clone(),
                        table
                            .rows
                            .iter()
                            .map(|cells| {
                                cells
                                    .iter()
                                    .map(|cell| match cell {
                                        crate::sections::Cell::Text(text, _) => text.clone(),
                                        _ => panic!("process fixture requires text cells"),
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>(),
                    )),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(table(&overlay), table(&expected));
        assert!(table(&overlay).0.iter().sum::<usize>() + 5 <= overlay.cols);
        let text = render_text(&overlay, cols, rows);
        assert!(text.contains(&identity.0.to_string()));
        assert!(!text.contains("new sample"));
        let (changed, counts) = measure(|| overlay.reflow_geometry(next));
        assert!(!changed);
        assert_eq!(
            (
                counts.allocation_calls,
                counts.realloc_calls,
                counts.body_builds,
                counts.process_row_builds,
                counts.disk_row_builds
            ),
            (0, 0, 0, 0, 0)
        );
    }
}

#[test]
fn terminal_resize_preserves_manual_scroll_and_unpaused_cached_rows() {
    let model = model_with_n_procs(60);
    let hist = history(10, NOW_MS);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, Rect::full(160, 48));
    overlay.wheel(10);
    let scroll = overlay.scroll();
    let rows = overlay.proc_rows.clone();
    let (changed, counts) = measure(|| overlay.reflow_geometry(Rect::full(80, 24)));
    assert!(changed);
    assert!(!overlay.follow && !overlay.paused);
    assert_eq!(overlay.scroll(), scroll.min(overlay.scroll_max()));
    assert_eq!(overlay.proc_rows, rows);
    assert_eq!(
        (
            counts.process_row_builds,
            counts.disk_row_builds,
            counts.body_builds
        ),
        (0, 0, 1)
    );
}

#[test]
fn unrelated_tab_updates_do_not_derive_process_or_disk_rows() {
    let mut model = model_with_n_procs(64);
    model.sidebar_status.disk_sizes = model_with_n_worktrees(32).sidebar_status.disk_sizes;
    let mut hist = history(10, NOW_MS);
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Cpu, &model, &hist, screen);
    assert!(overlay.proc_rows.is_empty() && overlay.disk_rows.is_empty());
    model.stats.cpu_pct = Some(91);
    hist.push(&model.stats, NOW_MS + 500);
    let mut ctx = ctx_at(&hist, screen);
    ctx.now_ms += 500;
    let (changed, counts) = measure(|| overlay.refresh(&model, &ctx));
    assert!(changed, "visible graph keeps its existing cadence");
    assert_eq!((counts.process_row_builds, counts.disk_row_builds), (0, 0));
    assert_eq!(counts.body_builds, 1);
    assert_eq!(overlay.last_now_ms, NOW_MS + 500);
    assert!(overlay.proc_rows.is_empty() && overlay.disk_rows.is_empty());
}

#[test]
fn unchanged_processes_skip_rows_body_and_repaint_for_unrelated_stats() {
    let mut model = model_with_n_procs(64);
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    let before = render_text(&overlay, 160, 48);
    model.stats.cpu_pct = Some(99);
    model.stats.mem_gib = Some((1.0, 64.0));
    let mut ctx = ctx_at(&hist, screen);
    ctx.now_ms += 500;
    let (changed, counts) = measure(|| overlay.refresh(&model, &ctx));
    assert!(!changed);
    assert_eq!(
        (
            counts.process_row_builds,
            counts.disk_row_builds,
            counts.body_builds
        ),
        (0, 0, 0)
    );
    assert_eq!(render_text(&overlay, 160, 48), before);
    assert_eq!(overlay.last_now_ms, NOW_MS + 500);
}

#[test]
fn process_revision_covers_values_attribution_name_and_birth_not_only_pids() {
    let mut model = model_with_n_procs(8);
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    let changes: [fn(&mut thegn_metrics::ProcSample); 5] = [
        |p| p.cpu_pct = 97.0,
        |p| p.rss_bytes = 9_876_543,
        |p| p.owner = thegn_metrics::ProcOwner::Pane(73),
        |p| p.name = "replacement-name-世".into(),
        |p| p.start_time += 123,
    ];
    for change in changes {
        change(&mut model.procs.procs[0]);
        model.process_revision += 1;
        let (changed, counts) = measure(|| overlay.refresh(&model, &ctx_at(&hist, screen)));
        assert!(changed);
        assert_eq!(
            (
                counts.process_row_builds,
                counts.disk_row_builds,
                counts.body_builds
            ),
            (1, 0, 1)
        );
        let expected = &model.procs.procs[0];
        let row = overlay
            .proc_rows
            .iter()
            .find(|row| row.pid == expected.pid)
            .unwrap();
        assert_eq!(
            (
                row.start_time,
                row.cpu_pct,
                row.rss_bytes,
                row.owner,
                row.name.as_str()
            ),
            (
                expected.start_time,
                expected.cpu_pct,
                expected.rss_bytes,
                expected.owner,
                expected.name.as_str()
            )
        );
    }
    model.procs.primed = false;
    model.procs.total += 99;
    model.process_revision += 1;
    assert!(overlay.refresh(&model, &ctx_at(&hist, screen)));
    assert!(render_text(&overlay, 160, 48).contains("warming"));
    model.procs_disabled = true;
    let (changed, counts) = measure(|| overlay.refresh(&model, &ctx_at(&hist, screen)));
    assert!(changed);
    assert_eq!(
        counts.process_row_builds, 0,
        "display policy does not require a re-sort"
    );
    assert!(render_text(&overlay, 160, 48).contains("process sampling is off"));
}

#[test]
fn process_view_changes_invalidate_order_but_geometry_only_rebuilds_body() {
    let mut model = model_with_n_procs(8);
    model.procs.procs[1].ppid = Some(model.procs.procs[0].pid);
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    overlay.prefs.proc_tree = true;
    let (_, counts) = measure(|| overlay.rebuild_after_key(&model, &ctx_at(&hist, screen)));
    assert_eq!(counts.process_row_builds, 1);
    assert!(overlay.proc_rows.iter().any(|row| row.depth > 0));
    overlay.filter = model.procs.procs[1].pid.to_string();
    let (_, counts) = measure(|| overlay.rebuild_after_key(&model, &ctx_at(&hist, screen)));
    assert_eq!(counts.process_row_builds, 1);
    assert!(
        overlay
            .proc_rows
            .iter()
            .any(|row| row.pid == model.procs.procs[1].pid)
    );
    overlay.prefs.proc_tree = false;
    overlay.prefs.proc_sort = ProcSort::Rss;
    overlay.prefs.proc_desc = !overlay.prefs.proc_desc;
    let (_, counts) = measure(|| overlay.rebuild_after_key(&model, &ctx_at(&hist, screen)));
    assert_eq!(counts.process_row_builds, 1);
    assert_eq!(overlay.proc_rows.len(), 1);
    let small = Rect::full(80, 24);
    let (changed, counts) = measure(|| overlay.refresh(&model, &ctx_at(&hist, small)));
    assert!(changed);
    assert_eq!((counts.process_row_builds, counts.body_builds), (0, 1));
    assert!(cursor_on_screen(&overlay));
    let selected = &overlay.proc_rows[overlay.sel];
    let identity = (selected.pid, selected.start_time);
    overlay.begin_signal();
    assert!(
        matches!(overlay.confirm, Some(super::super::Confirm::Signal {pid, start_time, ..}) if (pid,start_time) == identity)
    );
}

#[test]
fn process_header_coverage_and_capability_changes_do_not_resort_rows() {
    let mut model = model_with_n_procs(8);
    let hist = history(10, NOW_MS);
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    let before = overlay.coverage_note();
    let mut later = ctx_at(&hist, screen);
    later.now_ms += 1_000;
    let (changed, counts) = measure(|| overlay.refresh(&model, &later));
    assert!(changed, "existing visible coverage text advances");
    assert_ne!(overlay.coverage_note(), before);
    assert_eq!((counts.process_row_builds, counts.body_builds), (0, 0));
    later.now_ms += 100;
    assert!(
        !overlay.refresh(&model, &later),
        "subsecond change does not alter the label"
    );
    model.stats.gpu_pct = None;
    let (changed, counts) = measure(|| overlay.refresh(&model, &later));
    assert!(changed, "visible tab capability update repaints");
    assert_eq!(counts.process_row_builds, 0);
    assert!(!overlay.tabs.contains(&MonitorTab::Gpu));
}

#[test]
fn disk_age_and_graph_time_advance_without_rebuilding_cached_order() {
    let mut model = model_with_n_worktrees(8);
    for path in model.sidebar_status.disk_sizes.keys() {
        model.sidebar_status.disk_stamps.insert(path.clone(), 100);
    }
    let hist = history(10, NOW_MS);
    let screen = Rect::full(160, 48);
    let mut overlay = open_tab(MonitorTab::Disk, &model, &hist, screen);
    let paths: Vec<_> = overlay
        .disk_rows
        .iter()
        .map(|row| row.path.clone())
        .collect();
    assert_eq!(overlay.disk_rows[0].age_secs, Some(20));
    let mut later = ctx_at(&hist, screen);
    later.now_ms += 50_000;
    let (changed, counts) = measure(|| overlay.refresh(&model, &later));
    assert!(changed);
    assert_eq!(
        (
            counts.process_row_builds,
            counts.disk_row_builds,
            counts.body_builds
        ),
        (0, 0, 1)
    );
    assert_eq!(
        overlay
            .disk_rows
            .iter()
            .map(|row| &row.path)
            .collect::<Vec<_>>(),
        paths.iter().collect::<Vec<_>>()
    );
    assert_eq!(overlay.disk_rows[0].age_secs, Some(70));
    assert_eq!(overlay.last_now_ms, NOW_MS + 50_000);
    let changed_path = paths.last().unwrap().to_str().unwrap().to_owned();
    let mut hydrated = model.clone();
    hydrated
        .sidebar_status
        .disk_sizes
        .insert(changed_path.clone(), (99 << 30, 4 << 30));
    hydrated
        .sidebar_status
        .disk_stamps
        .insert(changed_path.clone(), 165);
    hydrated.carry_monitor_state_from(&mut model);
    model = hydrated;
    let (_, counts) = measure(|| overlay.refresh(&model, &later));
    assert_eq!(counts.disk_row_builds, 1);
    assert_eq!(
        overlay.disk_rows[0].path.to_str(),
        Some(changed_path.as_str())
    );
    assert_eq!(overlay.disk_rows[0].age_secs, Some(5));
    let mut same = model.clone();
    same.status = "unrelated hydration status".into();
    same.carry_monitor_state_from(&mut model);
    let (_, counts) = measure(|| overlay.refresh(&same, &later));
    assert_eq!(counts.disk_row_builds, 0);
}

#[test]
fn queued_worker_sample_is_revoked_before_paused_navigation_and_confirmation() {
    use crate::proc_worker::tests::{Event, Fixture};
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut model = model_with_n_procs(3);
    model.process_revision = 41;
    let before = model.procs.clone();
    let identity = (before.procs[0].pid, before.procs[0].start_time);
    let mut late = before.clone();
    late.procs[0].start_time += 500;
    late.procs[0].name = "late-replacement".into();
    let (release, gate) = std::sync::mpsc::channel();
    let fixture = Fixture::with_blocked_snapshot(gate, release.clone(), late);
    fixture.event(Event::Wait(None));
    fixture.control().set_enabled(true);
    fixture.event(Event::Sample);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    assert_eq!(ch(&mut overlay, ' '), MonitorOutcome::PrefsChanged);
    assert!(overlay.is_paused());
    // This is the real worker's pending publication: emulate it completing
    // between the pause key and the next compositor admission boundary.
    release.send(()).unwrap();
    fixture.event(Event::Wake);
    fixture.event(Event::Wait(Some(2_000)));
    assert!(
        !model
            .take_process_publication(fixture.control(), wants_process_scan(Some(&overlay), true))
    );
    assert_eq!(model.procs, before);
    assert_eq!(model.process_revision, 41);
    press(&mut overlay, &model, &hist, screen, '/');
    for c in identity.0.to_string().chars() {
        press(&mut overlay, &model, &hist, screen, c);
    }
    overlay.handle_key(&KeyCode::Enter, NONE);
    overlay.rebuild_after_key(&model, &ctx_at(&hist, screen));
    press(&mut overlay, &model, &hist, screen, 'j');
    assert_eq!(overlay.proc_rows.len(), 1);
    let row = &overlay.proc_rows[overlay.sel];
    assert_eq!((row.pid, row.start_time), identity);
    assert_ne!(row.name, "late-replacement");
    overlay.begin_signal();
    assert!(
        matches!(overlay.confirm, Some(super::super::Confirm::Signal {pid,start_time,..}) if (pid,start_time) == identity)
    );
    assert_eq!(fixture.settle(), crate::proc_worker::Settlement::Settled);
}

#[test]
fn reopening_after_process_view_invalidation_waits_for_a_new_snapshot() {
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut model = model_with_n_procs(2);
    let mut overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    assert!(!overlay.proc_rows.is_empty());

    model.invalidate_processes();
    assert!(overlay.refresh(&model, &ctx_at(&hist, screen)));
    assert!(overlay.proc_rows.is_empty());
    assert!(headings(&overlay)[0].0.contains("sampling"));
}

#[test]
fn reopening_after_terminal_process_failure_builds_the_error_body_immediately() {
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 48);
    let mut model = model_with_n_procs(2);
    let overlay = open_tab(MonitorTab::Procs, &model, &hist, screen);
    assert!(!render_text(&overlay, 160, 48).contains("unavailable"));

    // The loop re-applies its persistent terminal reason before constructing a
    // reopened overlay. The constructor must therefore render the error in its
    // initial body; waiting for a future sampler wake would leave "sampling…".
    model.invalidate_processes();
    model.fail_processes("sampler stopped");
    let reopened = open_tab(MonitorTab::Procs, &model, &hist, screen);
    let text = render_text(&reopened, 160, 48);
    assert!(text.contains("process sampling unavailable: sampler stopped"));
    assert!(!text.contains("sampling…"));
}

#[test]
fn equal_disk_sizes_and_basenames_have_deterministic_full_path_order() {
    let paths = [
        "/fixture/z/shared",
        "/fixture/a/shared",
        "/fixture/m/shared",
    ];
    let expected = [
        "/fixture/a/shared",
        "/fixture/m/shared",
        "/fixture/z/shared",
    ];
    for order in [paths, [paths[2], paths[1], paths[0]]] {
        let mut model = model_with_n_worktrees(0);
        for path in order {
            model
                .sidebar_status
                .disk_sizes
                .insert(path.into(), (1024, 512));
        }
        let rows = build::worktree_disk_rows(&model, 120);
        assert!(
            rows.iter()
                .all(|row| row.name == "shared" && row.total_bytes == 1024)
        );
        let actual: Vec<_> = rows.iter().map(|row| row.path.to_str().unwrap()).collect();
        assert_eq!(actual, expected);
    }
}
