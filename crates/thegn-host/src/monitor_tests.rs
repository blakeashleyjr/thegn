//! Tests for the system-monitor modal.
//!
//! Two invariants get disproportionate attention here because they are the ones
//! that fail silently rather than loudly:
//!
//! - **The scroll clamp.** `body_rows` must equal `rows - CHROME_ROWS`, or the
//!   tail of a tall tab becomes unreachable with no visible symptom.
//! - **Global keys stay global.** The Processes tab binds letters; if the
//!   per-tab arm ever ran first, `q` or `g` would silently stop working there.

use super::*;
use crate::telemetry::TelemetryHistory;
use termwiz::input::{KeyCode, Modifiers};
use thegn_metrics::StatsSnapshot;

const NONE: Modifiers = Modifiers::NONE;

/// A snapshot with every metric family present, so `visible()` yields all tabs.
fn full_snap() -> StatsSnapshot {
    StatsSnapshot {
        cpu_pct: Some(42),
        cpu_cores: vec![10, 90, 30, 70],
        cpu_freq_mhz: Some(3200),
        cpu_temp_c: Some(61.0),
        mem_gib: Some((8.0, 32.0)),
        swap_gib: Some((1.0, 8.0)),
        gpu_pct: Some(35),
        gpu_mem_mib: Some((2048, 8192)),
        gpu_temp_c: Some(55.0),
        gpu_power_w: Some(80.0),
        net_bps: Some((5_000, 900)),
        net_ifaces: vec![("eth0".into(), 5_000, 900)],
        battery: Some((77, false)),
        battery_power_w: Some(12.5),
        battery_eta_secs: Some(7200),
        disk_free_pct: Some(40),
        disk_bytes: Some((1 << 40, 400 << 30)),
        disks: vec![thegn_metrics::DiskInfo {
            name: "nvme0n1".into(),
            mount: "/".into(),
            free_pct: 40,
            read_bps: 1024,
            write_bps: 2048,
            kind: thegn_metrics::DiskKind::Ssd,
        }],
        temps: vec![("coretemp".into(), 61.0), ("nvme".into(), 44.0)],
        load_avg: Some((1.5, 1.2, 0.9)),
        uptime_secs: Some(90_000),
        self_rss_bytes: Some(64 << 20),
        self_cpu_pct: Some(3.5),
        ..Default::default()
    }
}

fn model_with(stats: StatsSnapshot) -> FrameModel {
    FrameModel {
        stats,
        // One owned container so the Containers tab is present in the "full"
        // fixtures (it hides when the list is empty — see the dedicated test).
        containers: vec![thegn_core::sandbox::ContainerInfo {
            name: "thegn-demo".into(),
            image: "debian".into(),
            status: "Up 2 minutes".into(),
            ours: true,
            backend: "docker".into(),
            cpu: "1.5%".into(),
            mem: "40MiB".into(),
            net: "1kB / 2kB".into(),
            containment: "worktree+caches".into(),
            mounts: String::new(),
        }],
        // One roster row so the Pipeline tab is present in the "full" fixtures,
        // for the same reason as the container above (it hides on an empty
        // roster with no configured pipeline — see the dedicated test).
        dispatches: crate::monitor_pipeline::DispatchRoster {
            rows: vec![thegn_core::issue::AgentDispatch {
                id: 1,
                issue_id: "THE-1".into(),
                worktree_path: "/wt/demo".into(),
                agent_name: "coder".into(),
                dispatched_at_ms: 60_000,
                status: thegn_core::issue::AgentDispatchStatus::Running,
                stage: Some("code".into()),
                parent_id: None,
                session_id: Some("s-1".into()),
                artifact_path: None,
            }],
            stages: vec![stage("code", None)],
        },
        // `[monitor] processes` is on by default; `FrameModel` derives its
        // default, so the fixture states it exactly as `build_model` does.
        procs_disabled: false,
        ..Default::default()
    }
}

/// One configured stage, as `stage_meta` would project it from
/// `[[pipeline.stages]]`.
fn stage(name: &str, next: Option<&str>) -> crate::monitor_pipeline::StageMeta {
    crate::monitor_pipeline::StageMeta {
        name: name.into(),
        agent: format!("{name}-agent"),
        concurrency: 2,
        next: next.map(str::to_string),
    }
}

/// The wall clock the fixtures pretend it is. `StatusCtx::new_for_test` pins
/// `now_ms` to 0, but a time window is resolved against real timestamps — so
/// every fixture has to agree on a "now" that its history actually precedes.
const NOW_MS: u64 = 120_000;

/// A `StatusCtx` whose clock matches the fixture history.
fn ctx_at<'a>(hist: &'a TelemetryHistory, screen: Rect) -> StatusCtx<'a> {
    let mut c = StatusCtx::new_for_test_on(hist, screen);
    c.now_ms = NOW_MS as i64;
    c
}

/// A history with `n` samples one second apart, ending at `t_end` ms.
fn history(n: u64, t_end: u64) -> TelemetryHistory {
    let mut h = TelemetryHistory::default();
    for i in 0..n {
        let mut s = full_snap();
        s.cpu_pct = Some((i % 100) as u8);
        h.push(&s, t_end - (n - 1 - i) * 1000);
    }
    h
}

/// Open a monitor on a 120×40 screen with a full snapshot.
fn open() -> (MonitorOverlay, FrameModel, TelemetryHistory) {
    open_on(Rect::full(120, 40), full_snap())
}

fn open_on(screen: Rect, stats: StatsSnapshot) -> (MonitorOverlay, FrameModel, TelemetryHistory) {
    let hist = history(120, NOW_MS);
    let model = model_with(stats);
    let ov = {
        let ctx = ctx_at(&hist, screen);
        MonitorOverlay::open(MonitorTab::Cpu, MonitorPrefs::default(), &model, &ctx)
    };
    (ov, model, hist)
}

fn key(ov: &mut MonitorOverlay, k: KeyCode) -> MonitorOutcome {
    ov.handle_key(&k, NONE)
}

fn ch(ov: &mut MonitorOverlay, c: char) -> MonitorOutcome {
    ov.handle_key(&KeyCode::Char(c), NONE)
}

// --- Closing -------------------------------------------------------------

#[test]
fn close_keys_close_and_modified_keys_pass_through() {
    let (mut ov, _m, _h) = open();
    assert_eq!(ch(&mut ov, 'q'), MonitorOutcome::Close);
    assert_eq!(key(&mut ov, KeyCode::Escape), MonitorOutcome::Close);
    assert_eq!(
        ov.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
        MonitorOutcome::Close
    );
    // Alt/Super chords belong to the compositor — HANDED BACK, not swallowed.
    // Swallowing them is what made the chord that opens the monitor unable to
    // close it (and what let `Ctrl-g` close the monitor instead of locking
    // keys).
    assert_eq!(
        ov.handle_key(&KeyCode::Char('p'), Modifiers::ALT),
        MonitorOutcome::Passthrough
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('x'), Modifiers::SUPER),
        MonitorOutcome::Passthrough
    );
    // Including `Ctrl Alt …`, which is the layer the open chord lives in — the
    // CTRL arm used to claim it first.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('M'), Modifiers::CTRL | Modifiers::ALT),
        MonitorOutcome::Passthrough
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('g'), Modifiers::CTRL),
        MonitorOutcome::Passthrough,
        "Ctrl-g is the global key lock, not a close"
    );
    // A plain Ctrl chord the monitor doesn't implement is still consumed: the
    // modal owns the keyboard except where it explicitly doesn't.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('w'), Modifiers::CTRL),
        MonitorOutcome::Pending
    );
}

#[test]
fn switching_tabs_records_where_to_reopen() {
    // `MonitorPrefs::last_tab` is persisted and read back when the overlay
    // reopens — but nothing ever wrote it, so "reopen where you left off"
    // always reopened on CPU.
    let (mut ov, _m, _h) = open();
    assert_eq!(ov.prefs().last_tab, MonitorTab::Cpu);

    // Cycling writes it…
    let out = key(&mut ov, KeyCode::Tab);
    assert_eq!(ov.prefs().last_tab, ov.tab);
    assert_ne!(ov.prefs().last_tab, MonitorTab::Cpu);
    assert_eq!(
        out,
        MonitorOutcome::PrefsChanged,
        "the loop only persists prefs on this outcome"
    );

    // …and so does a digit jump.
    let out = ch(&mut ov, '3');
    assert_eq!(ov.prefs().last_tab, ov.tab);
    assert_eq!(ov.tab, ov.tabs[2]);
    assert_eq!(out, MonitorOutcome::PrefsChanged);
}

// --- Tabs ----------------------------------------------------------------

#[test]
fn tab_cycles_the_visible_list_and_wraps() {
    let (mut ov, _m, _h) = open();
    let tabs = ov.tabs.clone();
    assert_eq!(tabs.len(), MonitorTab::ALL.len(), "full snapshot: all tabs");
    for expect in tabs.iter().skip(1).chain(tabs.first()) {
        key(&mut ov, KeyCode::Tab);
        assert_eq!(ov.tab, *expect);
    }
    // Shift-Tab reverses.
    ov.handle_key(&KeyCode::Tab, Modifiers::SHIFT);
    assert_eq!(ov.tab, *tabs.last().unwrap());
    // h/l mirror the arrows.
    ch(&mut ov, 'l');
    assert_eq!(ov.tab, tabs[0]);
    ch(&mut ov, 'h');
    assert_eq!(ov.tab, *tabs.last().unwrap());
}

#[test]
fn a_tab_with_no_data_on_this_machine_is_hidden() {
    // A tab that renders nothing reads as broken; omitting it is honest.
    let bare = StatsSnapshot {
        cpu_pct: Some(10),
        mem_gib: Some((1.0, 8.0)),
        ..Default::default()
    };
    let visible = MonitorTab::visible(&bare, false, false);
    assert!(!visible.contains(&MonitorTab::Gpu));
    assert!(!visible.contains(&MonitorTab::Power));
    assert!(!visible.contains(&MonitorTab::Thermal));
    assert!(!visible.contains(&MonitorTab::Disk));
    assert!(visible.contains(&MonitorTab::Cpu));
    assert!(visible.contains(&MonitorTab::Memory));
    assert!(visible.contains(&MonitorTab::Procs));
    // One metric appearing brings its tab back.
    let mut with_gpu = bare.clone();
    with_gpu.gpu_pct = Some(1);
    assert!(MonitorTab::visible(&with_gpu, false, false).contains(&MonitorTab::Gpu));
    // Containers is hidden with no containers, present with at least one — the
    // "no engine, no tab" spec scenario.
    assert!(!MonitorTab::visible(&bare, false, false).contains(&MonitorTab::Containers));
    assert!(MonitorTab::visible(&bare, true, false).contains(&MonitorTab::Containers));
    // Same rule for the pipeline board: hidden until a roster row exists or a
    // pipeline is configured, so a user who never dispatched an agent never
    // sees an empty tab.
    assert!(!MonitorTab::visible(&bare, false, false).contains(&MonitorTab::Pipeline));
    assert!(MonitorTab::visible(&bare, false, true).contains(&MonitorTab::Pipeline));
}

#[test]
fn digits_index_the_visible_tabs_not_the_full_list() {
    // On a GPU-less machine `3` must mean the third tab you can SEE, not the
    // third declared one — otherwise the same key does different things on
    // different hardware.
    let bare = StatsSnapshot {
        cpu_pct: Some(10),
        mem_gib: Some((1.0, 8.0)),
        ..Default::default()
    };
    let (mut ov, _m, _h) = open_on(Rect::full(120, 40), bare);
    let tabs = ov.tabs.clone();
    ch(&mut ov, '3');
    assert_eq!(ov.tab, tabs[2]);
    assert_ne!(ov.tab, MonitorTab::ALL[2], "indexed ALL, not visible");
    // Out of range is a no-op, never a panic.
    let before = ov.tab;
    ch(&mut ov, '9');
    assert_eq!(ov.tab, before);
}

#[test]
fn opening_at_a_hidden_tab_falls_back_to_a_real_one() {
    let bare = StatsSnapshot {
        cpu_pct: Some(10),
        ..Default::default()
    };
    let hist = history(10, NOW_MS);
    let model = model_with(bare);
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    let ov = MonitorOverlay::open(MonitorTab::Gpu, MonitorPrefs::default(), &model, &ctx);
    assert_ne!(ov.tab, MonitorTab::Gpu);
    assert!(ov.tabs.contains(&ov.tab));
}

#[test]
fn a_tab_vanishing_under_the_user_re_homes_the_cursor() {
    // Unplug the GPU / pop the battery mid-session: the modal must not be left
    // pointing at a tab that no longer exists.
    let (mut ov, _m, hist) = open();
    ch(&mut ov, '6');
    let gone = model_with(StatsSnapshot {
        cpu_pct: Some(10),
        mem_gib: Some((1.0, 8.0)),
        ..Default::default()
    });
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    ov.refresh(&gone, &ctx);
    assert!(
        ov.tabs.contains(&ov.tab),
        "left on a vanished tab: {:?}",
        ov.tab
    );
}

// --- Toggles -------------------------------------------------------------

#[test]
fn graph_style_cycles_and_is_scoped_to_the_active_tab() {
    let (mut ov, _m, _h) = open();
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).style, GraphStyle::Area);
    assert_eq!(ch(&mut ov, 'g'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).style, GraphStyle::Line);
    ch(&mut ov, 'g');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).style, GraphStyle::Spark);
    ch(&mut ov, 'g');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).style, GraphStyle::Area);
    // Sibling tabs are untouched — the toggle is per-tab, not global.
    ch(&mut ov, 'g');
    assert_eq!(ov.prefs.tab(MonitorTab::Memory).style, GraphStyle::Area);
}

#[test]
fn scale_cycles_per_tab() {
    let (mut ov, _m, _h) = open();
    assert_eq!(ch(&mut ov, 's'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).scale, ScaleMode::Fixed);
    ch(&mut ov, 's');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).scale, ScaleMode::Log);
    ch(&mut ov, 's');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).scale, ScaleMode::Window);
    assert_eq!(ov.prefs.tab(MonitorTab::Network).scale, ScaleMode::Window);
}

#[test]
fn window_keys_saturate_rather_than_wrap() {
    // Wrapping from the widest back to the narrowest reads as the key having
    // glitched, not as a deliberate cycle.
    let (mut ov, _m, _h) = open();
    for _ in 0..10 {
        ch(&mut ov, ']');
    }
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::EVERYTHING);
    ch(&mut ov, ']');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::EVERYTHING);
    for _ in 0..10 {
        ch(&mut ov, '[');
    }
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::from_secs(30));
    ch(&mut ov, '[');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::from_secs(30));
}

// --- Pause ---------------------------------------------------------------

#[test]
fn pause_freezes_the_view_and_refresh_becomes_a_no_op() {
    let (mut ov, model, hist) = open();
    assert_eq!(ch(&mut ov, ' '), MonitorOutcome::PrefsChanged);
    assert!(ov.is_paused());

    let before = render_text(&ov, 120, 40);
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    assert!(!ov.refresh(&model, &ctx), "paused refresh must not repaint");
    assert_eq!(render_text(&ov, 120, 40), before, "frozen view changed");

    // Resuming restores live refresh.
    ch(&mut ov, ' ');
    assert!(!ov.is_paused());
    assert!(ov.refresh(&model, &ctx));
}

#[test]
fn pausing_reduces_the_sampling_gate_and_closing_clears_it() {
    // The 0%-idle contract: a paused monitor must not keep the ticker in fast
    // mode, and a closed one must not keep any gate on.
    let (mut ov, _m, _h) = open();
    assert!(wants_live_stats(false, Some(&ov)));
    ch(&mut ov, ' ');
    assert!(!wants_live_stats(false, Some(&ov)));
    assert!(!wants_live_stats(false, None));
    // The panel's telemetry section still forces it independently.
    assert!(wants_live_stats(true, None));
}

#[test]
fn a_wide_window_does_not_ask_for_fast_sampling() {
    // 500ms resolution buys nothing on an hour-wide plot, so it shouldn't cost
    // the wake rate either. Expressed against the ladder rather than a press
    // count, so adding a rung can't silently turn this into a no-op.
    let (mut ov, _m, _h) = open();
    assert!(ov.wants_live(), "the default window should sample live");
    while ov.prefs.tab(MonitorTab::Cpu).window != Window::EVERYTHING {
        ch(&mut ov, ']');
    }
    assert!(!ov.wants_live());
    // The boundary itself: 10m still earns the fast cadence, 30m does not.
    while ov.prefs.tab(MonitorTab::Cpu).window != Window::from_secs(600) {
        ch(&mut ov, '[');
    }
    assert!(ov.wants_live());
    ch(&mut ov, ']');
    assert_eq!(
        ov.prefs.tab(MonitorTab::Cpu).window,
        Window::from_secs(1800)
    );
    assert!(!ov.wants_live());
}

#[test]
fn process_scanning_is_gated_on_the_tab_the_pause_and_the_config() {
    let (mut ov, _m, _h) = open();
    // Not on the Processes tab: no scan.
    assert!(!wants_process_scan(Some(&ov), true));
    while ov.tab != MonitorTab::Procs {
        key(&mut ov, KeyCode::Tab);
    }
    assert!(wants_process_scan(Some(&ov), true));
    // Config kill switch wins.
    assert!(!wants_process_scan(Some(&ov), false));
    // So does pause.
    ch(&mut ov, ' ');
    assert!(!wants_process_scan(Some(&ov), true));
    // And a closed monitor never scans.
    assert!(!wants_process_scan(None, true));
}

// --- The global-vs-per-tab guard ----------------------------------------

#[test]
fn global_keys_stay_global_on_the_processes_tab() {
    // The Processes tab binds letters for sorting. If the per-tab arm ever ran
    // before the global one, `q` would stop closing and `g` would stop cycling
    // — silently, and only on that one tab.
    let (mut ov, _m, _h) = open();
    while ov.tab != MonitorTab::Procs {
        key(&mut ov, KeyCode::Tab);
    }
    assert_eq!(ov.tab, MonitorTab::Procs);

    assert_eq!(ch(&mut ov, 'q'), MonitorOutcome::Close);

    let style = ov.prefs.tab(MonitorTab::Procs).style;
    ch(&mut ov, 'g');
    assert_ne!(ov.prefs.tab(MonitorTab::Procs).style, style, "`g` shadowed");

    let scale = ov.prefs.tab(MonitorTab::Procs).scale;
    ch(&mut ov, 's');
    assert_ne!(ov.prefs.tab(MonitorTab::Procs).scale, scale, "`s` shadowed");

    let win = ov.prefs.tab(MonitorTab::Procs).window;
    ch(&mut ov, '[');
    assert_ne!(ov.prefs.tab(MonitorTab::Procs).window, win, "`[` shadowed");

    ch(&mut ov, ' ');
    assert!(ov.is_paused(), "`space` shadowed");
    ch(&mut ov, ' ');

    // ...and h/l still switch tabs rather than sorting.
    ch(&mut ov, 'h');
    assert_ne!(ov.tab, MonitorTab::Procs, "`h` shadowed");
}

#[test]
fn processes_sort_keys_work_and_only_there() {
    let (mut ov, _m, _h) = open();
    // On CPU, a sort letter is inert.
    assert_eq!(ch(&mut ov, 'c'), MonitorOutcome::Pending);
    assert_eq!(ov.prefs.proc_sort, ProcSort::Cpu);

    while ov.tab != MonitorTab::Procs {
        key(&mut ov, KeyCode::Tab);
    }
    assert_eq!(ch(&mut ov, 'm'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.prefs.proc_sort, ProcSort::Rss);
    ch(&mut ov, 'n');
    assert_eq!(ov.prefs.proc_sort, ProcSort::Name);
    ch(&mut ov, 'c');
    assert_eq!(ov.prefs.proc_sort, ProcSort::Cpu);
    // `r` flips direction without changing the column.
    assert!(ov.prefs.proc_desc);
    ch(&mut ov, 'r');
    assert!(!ov.prefs.proc_desc);
    assert_eq!(ov.prefs.proc_sort, ProcSort::Cpu);
}

// --- Geometry and scrolling ---------------------------------------------

#[test]
fn the_box_reserves_exactly_the_chrome_rows() {
    // If `body_rows` and the renderer disagree, the tail of a tall tab becomes
    // unreachable with no visible symptom.
    for (w, h) in [(120usize, 40usize), (80, 24), (60, 18), (40, 12)] {
        let (ov, _m, _h) = open_on(Rect::full(w, h), full_snap());
        assert_eq!(ov.body_rows, ov.rows - CHROME_ROWS, "{w}x{h}");
        assert!(ov.cols <= w.saturating_sub(6).max(1), "{w}x{h}");
        assert!(ov.rows <= h.saturating_sub(3).max(1), "{w}x{h}");
    }
}

#[test]
fn scrolling_reaches_the_last_row_and_stops_there() {
    // A short screen makes any tab overflow.
    let (mut ov, _m, _h) = open_on(Rect::full(80, 14), full_snap());
    let max = ov.scroll_max();
    assert!(max > 0, "expected overflow on a short screen");
    for _ in 0..200 {
        ch(&mut ov, 'j');
    }
    assert_eq!(
        ov.scroll(),
        max,
        "scrolled past or stopped short of the end"
    );
    for _ in 0..200 {
        ch(&mut ov, 'k');
    }
    assert_eq!(ov.scroll(), 0);
    // End/Home are the same journey in one key.
    key(&mut ov, KeyCode::End);
    assert_eq!(ov.scroll(), max);
    key(&mut ov, KeyCode::Home);
    assert_eq!(ov.scroll(), 0);
}

#[test]
fn spark_style_shrinks_the_body_and_clamps_the_viewport() {
    // Proves `Section::height` honours the style: if it didn't, the scroll
    // offset would survive past the end of a now-shorter stack.
    let (mut ov, _m, _h) = open_on(Rect::full(80, 14), full_snap());
    key(&mut ov, KeyCode::End);
    let tall = ov.scroll_max();
    // Area -> Line -> Spark.
    ch(&mut ov, 'g');
    ch(&mut ov, 'g');
    assert_eq!(ov.prefs.tab(ov.tab).style, GraphStyle::Spark);
    ov.sync(&_m, &_h, Rect::full(80, 14));
    assert!(ov.scroll_max() < tall, "spark should shorten the stack");
    assert!(
        ov.scroll() <= ov.scroll_max(),
        "viewport stranded past the end"
    );
}

#[test]
fn every_tab_builds_and_scrolls_without_panicking() {
    // Cheap coverage that no builder indexes past an absent metric.
    for stats in [full_snap(), StatsSnapshot::default()] {
        let (mut ov, _m, _h) = open_on(Rect::full(70, 20), stats);
        for _ in 0..MonitorTab::ALL.len() {
            key(&mut ov, KeyCode::Tab);
            for _ in 0..40 {
                ch(&mut ov, 'j');
            }
            key(&mut ov, KeyCode::PageUp);
            assert!(ov.scroll() <= ov.scroll_max());
        }
    }
}

// --- Pipeline board ------------------------------------------------------

/// Move the overlay onto a tab by name (the digit keys index the *visible*
/// list, which is exactly what a user does).
fn goto(ov: &mut MonitorOverlay, model: &FrameModel, hist: &TelemetryHistory, want: MonitorTab) {
    for _ in 0..MonitorTab::ALL.len() {
        if ov.tab == want {
            let ctx = ctx_at(hist, Rect::full(120, 40));
            ov.rebuild_after_key(model, &ctx);
            return;
        }
        key(ov, KeyCode::Tab);
    }
    panic!("{want:?} was never reached by tab-cycling");
}

/// The board must be reachable with the keys the overlay already has — no new
/// action, no new keybind. This is the assertion the change's "no action
/// checklist applies" claim rests on.
#[test]
fn the_pipeline_board_is_reachable_by_tab_cycling_and_by_its_digit() {
    let (mut ov, model, hist) = open();
    goto(&mut ov, &model, &hist, MonitorTab::Pipeline);
    assert_eq!(ov.tab, MonitorTab::Pipeline);

    // …and by the digit that indexes it in the VISIBLE list. The digit keys are
    // `1`-`9`, so this holds whenever the board sits in the first nine visible
    // tabs — i.e. on any machine that hides at least one hardware tab, which is
    // most of them. On a machine showing all ten, `Tab` above is the way in.
    let bare = StatsSnapshot {
        cpu_pct: Some(10),
        mem_gib: Some((1.0, 8.0)),
        ..Default::default()
    };
    let (mut ov, model, hist) = open_on(Rect::full(120, 40), bare);
    let ix = ov
        .tabs
        .iter()
        .position(|t| *t == MonitorTab::Pipeline)
        .expect("pipeline visible with a roster row");
    assert!(
        ix < 9,
        "the board must be digit-reachable on a plain machine"
    );
    ch(
        &mut ov,
        char::from_digit(ix as u32 + 1, 10).expect("a digit"),
    );
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    ov.rebuild_after_key(&model, &ctx);
    assert_eq!(ov.tab, MonitorTab::Pipeline);
}

#[test]
fn the_board_renders_its_stage_group_and_row() {
    let (mut ov, model, hist) = open();
    goto(&mut ov, &model, &hist, MonitorTab::Pipeline);
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("agent pipeline"), "header missing: {text}");
    assert!(text.contains("code"), "stage group heading missing: {text}");
    assert!(text.contains("coder"), "agent name missing: {text}");
    assert!(text.contains("demo"), "worktree basename missing: {text}");
    // The read-only legend, not the graph toggles.
    assert!(
        text.contains("go to worktree"),
        "board legend missing: {text}"
    );
}

#[test]
fn enter_on_a_board_row_raises_a_jump_for_that_worktree() {
    let (mut ov, model, hist) = open();
    goto(&mut ov, &model, &hist, MonitorTab::Pipeline);
    assert_eq!(key(&mut ov, KeyCode::Enter), MonitorOutcome::Action);
    assert_eq!(
        ov.take_action(),
        Some(crate::monitor::MonitorAction::Pipeline(
            crate::monitor::PipelineJump {
                worktree: "/wt/demo".into(),
                session: Some("s-1".into()),
            }
        ))
    );
    // Drained exactly once.
    assert_eq!(ov.take_action(), None);
}

#[test]
fn the_board_samples_only_while_it_is_the_live_view() {
    let (mut ov, model, hist) = open();
    assert!(!ov.wants_dispatches(), "another tab must not sample");
    goto(&mut ov, &model, &hist, MonitorTab::Pipeline);
    assert!(ov.wants_dispatches());
    // Pausing freezes the view, so it stops paying for samples too.
    ch(&mut ov, ' ');
    assert!(!ov.wants_dispatches());
}

// --- Rendering -----------------------------------------------------------

fn render_text(ov: &MonitorOverlay, w: usize, h: usize) -> String {
    let mut s = Surface::new(w, h);
    ov.render(&mut s, Rect::full(w, h));
    s.screen_chars_to_string()
}

#[test]
fn the_tab_bar_and_footer_are_drawn() {
    let (ov, _m, _h) = open();
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("CPU"), "active tab label missing: {text}");
    assert!(text.contains("Memory"), "inactive tab label missing");
    assert!(text.contains("Network"));
    // Footer hints.
    assert!(text.contains("tabs"), "footer hints missing: {text}");
    assert!(text.contains("pause"));
    assert!(text.contains("close"));
}

#[test]
fn the_footer_reflects_the_live_toggles() {
    let (mut ov, model, hist) = open();
    ch(&mut ov, 'g');
    ch(&mut ov, ']');
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    ov.refresh(&model, &ctx);
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("line"), "style not shown: {text}");
    // Whatever rung `]` actually landed on — asserting a literal would pin the
    // test to the shipped ladder rather than to the footer's behavior.
    let want = ov.prefs.tab(MonitorTab::Cpu).window.label();
    assert!(text.contains(&want), "window {want} not shown: {text}");
}

#[test]
fn pausing_is_visible_in_the_chrome() {
    // A frozen monitor that doesn't say so is indistinguishable from a hung one.
    let (mut ov, _m, _h) = open();
    ch(&mut ov, ' ');
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("paused"), "no pause indicator: {text}");
    assert!(text.contains("resume"), "footer still says pause: {text}");
}

#[test]
fn a_short_window_over_thin_history_admits_how_much_it_has() {
    // A 1h axis over 20s of data must not imply the rest was flat.
    let hist = history(20, NOW_MS);
    let model = model_with(full_snap());
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    let mut ov = MonitorOverlay::open(MonitorTab::Cpu, MonitorPrefs::default(), &model, &ctx);
    while ov.prefs.tab(MonitorTab::Cpu).window != Window::from_secs(3600) {
        ch(&mut ov, ']');
    }
    ov.refresh(&model, &ctx);
    let text = render_text(&ov, 120, 40);
    assert!(
        text.contains("of history"),
        "coverage not disclosed: {text}"
    );
}

#[test]
fn per_core_utilization_is_shown() {
    // The sampler has always collected `cpu_cores`; the popups never showed it.
    let (ov, _m, _h) = open();
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("cores"), "no per-core block: {text}");
}

#[test]
fn every_thermal_sensor_is_listed_not_just_the_hottest() {
    let (mut ov, _m, _h) = open();
    while ov.tab != MonitorTab::Thermal {
        key(&mut ov, KeyCode::Tab);
    }
    ov.sync(&_m, &_h, Rect::full(120, 40));
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("coretemp"), "{text}");
    assert!(text.contains("nvme"), "secondary sensor hidden: {text}");
}

#[test]
fn the_processes_tab_explains_itself_before_the_first_sample() {
    // An empty table reads as broken; "sampling…" reads as working.
    let (mut ov, _m, _h) = open();
    while ov.tab != MonitorTab::Procs {
        key(&mut ov, KeyCode::Tab);
    }
    ov.sync(&_m, &_h, Rect::full(120, 40));
    let text = render_text(&ov, 120, 40);
    assert!(
        text.contains("sampling") || text.contains("processes"),
        "no explanation on an empty Processes tab: {text}"
    );
}

#[test]
fn the_box_rect_encloses_what_was_drawn() {
    // The mouse hit target must match the painted box, or an outside-click
    // dismiss fires on a click that looked inside.
    let (ov, _m, _h) = open();
    let screen = Rect::full(120, 40);
    let r = ov.box_rect(screen).expect("a box");
    assert!(r.cols >= ov.cols && r.rows >= ov.rows);
    assert!(r.x + r.cols <= screen.cols && r.y + r.rows <= screen.rows);
}

// --- Widget → tab mapping ------------------------------------------------

#[test]
fn every_stat_widget_maps_to_a_tab_that_claims_it_back() {
    // The two id lists live apart (`detail::widget_detail` dispatches on one,
    // `MonitorTab::widget_id` reports the other). Pin them together.
    for t in MonitorTab::ALL {
        if let Some(w) = t.widget_id() {
            assert_eq!(
                MonitorTab::for_widget(w),
                Some(t),
                "{t:?} claims widget {w:?} but it maps elsewhere"
            );
        }
    }
    // Widgets that share a tab rather than owning one.
    assert_eq!(MonitorTab::for_widget("swap"), Some(MonitorTab::Memory));
    assert_eq!(MonitorTab::for_widget("load"), Some(MonitorTab::Cpu));
    assert_eq!(MonitorTab::for_widget("freq"), Some(MonitorTab::Cpu));
    // Non-metric widgets expand into nothing.
    assert_eq!(MonitorTab::for_widget("clock"), None);
    assert_eq!(MonitorTab::for_widget("pr"), None);
}

#[test]
fn tab_indices_match_declaration_order() {
    // Prefs are stored in a positional array; a stale `index()` would swap two
    // tabs' saved settings.
    for (i, t) in MonitorTab::ALL.iter().enumerate() {
        assert_eq!(t.index(), i, "{t:?}");
    }
    // Persistence slugs are unique and round-trip.
    for t in MonitorTab::ALL {
        assert_eq!(MonitorTab::from_key(t.key()), Some(t));
    }
}

impl MonitorOverlay {
    /// What the loop does after a key the monitor consumed: rebuild the body so
    /// a tab switch or a toggle is on screen immediately, not one sample later.
    #[cfg(test)]
    fn sync(&mut self, model: &FrameModel, hist: &TelemetryHistory, screen: Rect) {
        self.rebuild_after_key(model, &ctx_at(hist, screen));
    }
}

// --- Processes: filter / tree / signal ----------------------------------

fn proc(pid: u32, ppid: Option<u32>, name: &str, cpu: f32, rss: u64) -> thegn_metrics::ProcSample {
    thegn_metrics::ProcSample {
        pid,
        ppid,
        name: name.into(),
        cpu_pct: cpu,
        rss_bytes: rss,
        run_secs: 0,
        owner: thegn_metrics::ProcOwner::Other,
    }
}

fn model_with_procs(procs: Vec<thegn_metrics::ProcSample>) -> FrameModel {
    let mut m = model_with(full_snap());
    m.procs = thegn_metrics::ProcSnapshot {
        total: procs.len(),
        procs,
        primed: true,
        enabled: true,
    };
    m
}

fn goto_procs(ov: &mut MonitorOverlay) {
    while ov.tab != MonitorTab::Procs {
        key(ov, KeyCode::Tab);
    }
}

#[test]
fn slash_opens_the_filter_and_esc_clears_it_without_closing() {
    let (mut ov, _m, _h) = open();
    goto_procs(&mut ov);
    assert_eq!(ch(&mut ov, '/'), MonitorOutcome::Pending);
    assert!(ov.filtering);
    // Typed letters land in the query, not the global handlers (`q`/`g`/`s`).
    for c in ['c', 'a', 'r', 'g'] {
        assert_eq!(ch(&mut ov, c), MonitorOutcome::Pending);
    }
    assert_eq!(ov.filter, "carg");
    assert!(ov.filtering);
    // Backspace edits.
    key(&mut ov, KeyCode::Backspace);
    assert_eq!(ov.filter, "car");
    // Enter applies and leaves input mode, keeping the query.
    key(&mut ov, KeyCode::Enter);
    assert!(!ov.filtering && ov.filter == "car");
    // Esc while filtering cancels the filter — it must NOT close the monitor.
    ch(&mut ov, '/');
    assert_eq!(key(&mut ov, KeyCode::Escape), MonitorOutcome::Pending);
    assert!(!ov.filtering && ov.filter.is_empty());
}

#[test]
fn filter_narrows_the_displayed_rows() {
    let screen = Rect::full(120, 40);
    let model = model_with_procs(vec![
        proc(100, None, "cargo", 90.0, 1),
        proc(200, None, "zsh", 1.0, 1),
    ]);
    let hist = history(120, NOW_MS);
    let mut ov = {
        let ctx = ctx_at(&hist, screen);
        MonitorOverlay::open(MonitorTab::Procs, MonitorPrefs::default(), &model, &ctx)
    };
    ov.sync(&model, &hist, screen);
    assert_eq!(ov.proc_rows.len(), 2);
    ch(&mut ov, '/');
    ch(&mut ov, 'z');
    ov.sync(&model, &hist, screen);
    assert_eq!(ov.proc_rows.len(), 1);
    assert_eq!(ov.proc_rows[0].pid, 200);
}

#[test]
fn t_toggles_tree_grouping() {
    let (mut ov, _m, _h) = open();
    goto_procs(&mut ov);
    assert!(!ov.prefs.proc_tree);
    assert_eq!(ch(&mut ov, 't'), MonitorOutcome::PrefsChanged);
    assert!(ov.prefs.proc_tree);
}

#[test]
fn signal_confirms_then_surfaces_failure_never_swallows_it() {
    let screen = Rect::full(120, 40);
    // A pid that cannot exist (well above any real pid, still < i32::MAX so the
    // guard doesn't reject it): the signal fails with ESRCH and is surfaced,
    // and nothing real is ever touched.
    let bogus = 2_000_000_000u32;
    let model = model_with_procs(vec![proc(bogus, None, "ghost", 5.0, 1)]);
    let hist = history(120, NOW_MS);
    let mut ov = {
        let ctx = ctx_at(&hist, screen);
        MonitorOverlay::open(MonitorTab::Procs, MonitorPrefs::default(), &model, &ctx)
    };
    ov.sync(&model, &hist, screen);
    // `x` opens a confirmation rather than firing.
    ch(&mut ov, 'x');
    assert!(ov.confirm.is_some());
    // `n` cancels with nothing sent.
    ch(&mut ov, 'n');
    assert!(ov.confirm.is_none());
    // `x` then `y` performs, and the failure is shown, not swallowed.
    ch(&mut ov, 'x');
    ch(&mut ov, 'y');
    assert!(
        ov.status.as_deref().unwrap_or("").contains("ghost"),
        "{:?}",
        ov.status
    );
}

#[test]
fn disk_clean_confirms_and_queues_an_off_loop_action() {
    let screen = Rect::full(120, 40);
    let mut model = model_with(full_snap());
    model
        .sidebar_status
        .disk_sizes
        .insert("/tmp/wt/feature".to_string(), (5 << 30, 3 << 30));
    let hist = history(120, NOW_MS);
    let mut ov = {
        let ctx = ctx_at(&hist, screen);
        MonitorOverlay::open(MonitorTab::Disk, MonitorPrefs::default(), &model, &ctx)
    };
    ov.sync(&model, &hist, screen);
    assert_eq!(ov.disk_rows.len(), 1);
    // `x` on the Disk tab confirms a clean; `y` queues it for the loop.
    ch(&mut ov, 'x');
    assert!(matches!(ov.confirm, Some(super::Confirm::Clean { .. })));
    ch(&mut ov, 'y');
    assert_eq!(
        ov.take_action(),
        Some(super::MonitorAction::CleanWorktree(
            std::path::PathBuf::from("/tmp/wt/feature")
        ))
    );
}

// --- The row cursor and the viewport that follows it ---------------------
//
// These are the tests for the safety property, not for a look: `x` on
// Processes signals `proc_rows[sel]` and `x` on Disk cleans `disk_rows[sel]`,
// so the row the destructive key targets MUST be the row on screen and the row
// wearing the cursor. Before this, `nav` moved the cursor and raw-scrolled the
// viewport by the same delta against a different clamp, so the two drifted
// apart within a few keystrokes.

/// A short box, so any list overflows its viewport.
const SHORT: Rect = Rect {
    x: 0,
    y: 0,
    cols: 80,
    rows: 18,
};

/// What the loop does after every consumed key: press, then rebuild.
fn press(
    ov: &mut MonitorOverlay,
    model: &FrameModel,
    hist: &TelemetryHistory,
    screen: Rect,
    c: char,
) {
    ch(ov, c);
    ov.sync(model, hist, screen);
}

/// Open directly on `tab` against `model`, already synced.
fn open_tab(
    tab: MonitorTab,
    model: &FrameModel,
    hist: &TelemetryHistory,
    screen: Rect,
) -> MonitorOverlay {
    let mut ov = {
        let ctx = ctx_at(hist, screen);
        MonitorOverlay::open(tab, MonitorPrefs::default(), model, &ctx)
    };
    ov.sync(model, hist, screen);
    ov
}

/// `n` processes, enough to overflow any test viewport.
fn model_with_n_procs(n: u32) -> FrameModel {
    model_with_procs(
        (0..n)
            .map(|i| {
                proc(
                    1000 + i,
                    None,
                    &format!("proc{i:02}"),
                    90.0 - i as f32,
                    1 << 20,
                )
            })
            .collect(),
    )
}

/// `n` worktrees on the Disk tab, distinct sizes so the sort order is stable.
fn model_with_n_worktrees(n: u64) -> FrameModel {
    let mut m = model_with(full_snap());
    for i in 0..n {
        m.sidebar_status.disk_sizes.insert(
            format!("/tmp/wt/w{i:02}"),
            (((n - i) << 30) as i64, 1 << 30),
        );
    }
    m
}

/// The row the cursor is on, as the viewport sees it.
fn cursor_on_screen(ov: &MonitorOverlay) -> bool {
    let Some(&y) = ov.row_y.get(ov.sel) else {
        return false;
    };
    y >= ov.scroll() && y < ov.scroll() + ov.body_rows
}

/// The one table in the built body that carries a row cursor.
fn cursor_table(ov: &MonitorOverlay) -> &crate::sections::TableSection {
    ov.body
        .iter()
        .find_map(|s| match s {
            Section::Table(t) if t.sel.is_some() => Some(t),
            _ => None,
        })
        .expect("a table with a row cursor")
}

/// How many tables in the built body claim a cursor.
fn cursor_tables(ov: &MonitorOverlay) -> usize {
    ov.body
        .iter()
        .filter(|s| matches!(s, Section::Table(t) if t.sel.is_some()))
        .count()
}

fn cell_tone(c: &crate::sections::Cell) -> Tok {
    match c {
        crate::sections::Cell::Text(_, t) | crate::sections::Cell::Bar(_, _, t) => *t,
    }
}

#[test]
fn the_viewport_follows_the_row_cursor_down_a_long_list() {
    let model = model_with_n_procs(30);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, SHORT);
    assert!(
        ov.row_y.len() >= ov.body_rows + 5,
        "fixture must overflow the viewport: {} rows in {} visible",
        ov.row_y.len(),
        ov.body_rows
    );
    // Walk the cursor well past the fold; it must be on screen at every step,
    // never just at the end.
    for step in 0..25 {
        press(&mut ov, &model, &hist, SHORT, 'j');
        assert_eq!(ov.sel, step + 1, "cursor did not advance");
        assert!(
            cursor_on_screen(&ov),
            "row {} at y={:?} fell outside [{}, {})",
            ov.sel,
            ov.row_y.get(ov.sel),
            ov.scroll(),
            ov.scroll() + ov.body_rows
        );
    }
    // …and back up again.
    for _ in 0..25 {
        press(&mut ov, &model, &hist, SHORT, 'k');
        assert!(cursor_on_screen(&ov));
    }
    assert_eq!(ov.sel, 0);
    // Following scrolls the MINIMUM distance, so row 0 sits at the top edge
    // rather than dragging the whole heading back into view.
    assert_eq!(ov.scroll(), ov.row_y[0]);
}

#[test]
fn scrolling_never_retargets_the_destructive_key() {
    // Processes: `x` signals `proc_rows[sel]`. Whatever the confirmation names
    // must be the row that is highlighted AND on screen.
    let model = model_with_n_procs(30);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, SHORT);
    for _ in 0..18 {
        press(&mut ov, &model, &hist, SHORT, 'j');
    }
    assert!(cursor_on_screen(&ov), "the targeted row scrolled away");
    let target = ov.proc_rows[ov.sel].clone();
    // The highlighted row is the same row.
    let t = cursor_table(&ov);
    assert_eq!(t.sel, Some(ov.sel), "the painted cursor is on another row");
    ch(&mut ov, 'x');
    match &ov.confirm {
        Some(super::Confirm::Signal { pid, label, .. }) => {
            assert_eq!(*pid, target.pid);
            assert!(
                label.contains(&target.name),
                "the prompt names {label}, the cursor is on {}",
                target.name
            );
        }
        other => panic!("expected a signal confirmation, got {other:?}"),
    }

    // Disk: `x` cleans `disk_rows[sel]`, and the lane sits below a graph, a
    // volumes table and a grid — the layout that made the old cursor and
    // viewport diverge fastest.
    let model = model_with_n_worktrees(20);
    let mut ov = open_tab(MonitorTab::Disk, &model, &hist, SHORT);
    for _ in 0..12 {
        press(&mut ov, &model, &hist, SHORT, 'j');
    }
    assert_eq!(ov.sel, 12);
    assert!(cursor_on_screen(&ov), "the targeted worktree scrolled away");
    assert_eq!(cursor_table(&ov).sel, Some(ov.sel));
    let target = ov.disk_rows[ov.sel].clone();
    ch(&mut ov, 'x');
    match &ov.confirm {
        Some(super::Confirm::Clean { path, label }) => {
            assert_eq!(*path, target.path);
            assert_eq!(*label, target.name);
        }
        other => panic!("expected a clean confirmation, got {other:?}"),
    }
}

#[test]
fn home_and_end_move_the_cursor_on_a_list_tab() {
    let model = model_with_n_procs(30);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, SHORT);
    key(&mut ov, KeyCode::End);
    ov.sync(&model, &hist, SHORT);
    assert_eq!(
        ov.sel,
        ov.proc_rows.len() - 1,
        "End must land on the last row"
    );
    assert!(cursor_on_screen(&ov), "End left the cursor off screen");
    key(&mut ov, KeyCode::Home);
    ov.sync(&model, &hist, SHORT);
    assert_eq!(ov.sel, 0);
    assert!(cursor_on_screen(&ov), "Home left the cursor off screen");
    assert_eq!(
        ov.scroll(),
        ov.row_y[0],
        "Home should rest at the first row"
    );
}

#[test]
fn a_wheel_scroll_stops_the_viewport_chasing_until_the_next_key() {
    let model = model_with_n_procs(30);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, SHORT);
    // Take the viewport by hand, then let a live sample land: it must stay put
    // rather than snap back to row 0's cursor.
    ov.wheel(6);
    let parked = ov.scroll();
    assert!(parked > 0, "the wheel should have moved the viewport");
    {
        let ctx = ctx_at(&hist, SHORT);
        ov.refresh(&model, &ctx);
    }
    assert_eq!(
        ov.scroll(),
        parked,
        "a refresh yanked the hand-set viewport"
    );
    // A cursor key re-arms following, and the viewport comes back to the cursor.
    press(&mut ov, &model, &hist, SHORT, 'j');
    assert!(ov.follow);
    assert!(cursor_on_screen(&ov));
    assert!(
        ov.scroll() < parked,
        "following should have pulled the viewport back to the cursor"
    );
}

#[test]
fn the_selected_row_is_the_only_one_with_a_selection_background() {
    let hist = history(120, NOW_MS);
    // Processes and Containers: one table each, one cursor.
    let model = model_with_n_procs(8);
    let ov = open_tab(MonitorTab::Procs, &model, &hist, SHORT);
    assert_eq!(cursor_tables(&ov), 1, "processes");
    let model = model_with(full_snap());
    let ov = open_tab(MonitorTab::Containers, &model, &hist, SHORT);
    assert_eq!(cursor_tables(&ov), 1, "containers");
    // Disk: the volumes table sits above the worktree lane and must NOT claim
    // a cursor — only the lane `sel` indexes does.
    let model = model_with_n_worktrees(4);
    let ov = open_tab(MonitorTab::Disk, &model, &hist, SHORT);
    assert_eq!(cursor_tables(&ov), 1, "disk");
    // Pipeline: one table PER stage group, and only the group holding the
    // cursor carries it.
    let model = model_with_two_stages();
    let mut ov = open_tab(MonitorTab::Pipeline, &model, &hist, SHORT);
    assert!(
        ov.pipeline_rows.len() > 2,
        "fixture needs both groups filled"
    );
    for _ in 0..ov.pipeline_rows.len() {
        assert_eq!(cursor_tables(&ov), 1, "board at sel={}", ov.sel);
        // The global `row_y` must index the cursor across the group boundary.
        assert!(cursor_on_screen(&ov), "board row {} off screen", ov.sel);
        press(&mut ov, &model, &hist, SHORT, 'j');
    }
}

/// A roster spanning two stages, so the board emits two tables.
fn model_with_two_stages() -> FrameModel {
    let mut m = model_with(full_snap());
    let row = |id: i64, stage: &str, name: &str| thegn_core::issue::AgentDispatch {
        id,
        issue_id: format!("THE-{id}"),
        worktree_path: format!("/wt/{name}"),
        agent_name: name.into(),
        dispatched_at_ms: 60_000,
        status: thegn_core::issue::AgentDispatchStatus::Running,
        stage: Some(stage.into()),
        parent_id: None,
        session_id: None,
        artifact_path: None,
    };
    m.dispatches = crate::monitor_pipeline::DispatchRoster {
        rows: vec![
            row(1, "code", "coder-a"),
            row(2, "code", "coder-b"),
            row(3, "review", "reviewer"),
        ],
        stages: vec![stage("code", Some("review")), stage("review", None)],
    };
    m
}

#[test]
fn selecting_a_container_row_keeps_its_ownership_tint() {
    // The regression the audit called out: the cursor used to REPLACE the
    // ownership tint, destroying the ours/foreign signal for exactly the row
    // the user was about to act on. Selection is a background now.
    let mut model = model_with(full_snap());
    model.containers.push(thegn_core::sandbox::ContainerInfo {
        name: "someone-elses".into(),
        image: "nginx".into(),
        status: "Up 3 hours".into(),
        ours: false,
        backend: "docker".into(),
        cpu: "0.1%".into(),
        mem: "9MiB".into(),
        net: "0B / 0B".into(),
        containment: String::new(),
        mounts: String::new(),
    });
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Containers, &model, &hist, SHORT);
    // Row 0 is ours: green, cursor or not.
    assert_eq!(ov.sel, 0);
    let t = cursor_table(&ov);
    assert_eq!(t.sel, Some(0));
    assert_eq!(
        cell_tone(&t.rows[0][0]),
        Tok::Hue(thegn_core::theme::Hue::Green),
        "the selected owned row lost its ownership tint"
    );
    // Row 1 is foreign: ghost, cursor or not.
    press(&mut ov, &model, &hist, SHORT, 'j');
    assert_eq!(ov.sel, 1);
    let t = cursor_table(&ov);
    assert_eq!(t.sel, Some(1));
    assert_eq!(cell_tone(&t.rows[1][0]), Tok::Slot(S::Ghost));
    assert_eq!(
        cell_tone(&t.rows[0][0]),
        Tok::Hue(thegn_core::theme::Hue::Green),
        "the unselected owned row changed tint"
    );
}

// --- Numbered tabs and a bar that never clips the active one -------------

/// The text a `Line` would draw, clusters concatenated.
fn line_text(l: &Line) -> String {
    match l {
        Line::Segs(v) => v.iter().map(|s| s.text.as_str()).collect(),
        Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
            l.iter().chain(r.iter()).map(|s| s.text.as_str()).collect()
        }
        _ => String::new(),
    }
}

#[test]
fn the_tenth_tab_is_reachable_by_zero() {
    // The fixture shows every family, so Pipeline is the tenth visible tab —
    // the exact case where the old `1`-`9` arm left it unreachable.
    let (mut ov, _m, _h) = open();
    assert_eq!(ov.tabs.len(), MonitorTab::ALL.len());
    assert_eq!(ov.tabs[9], MonitorTab::Pipeline);
    assert_eq!(ch(&mut ov, '0'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.tab, MonitorTab::Pipeline);
    // …and the bar says so, rather than making the user guess.
    assert!(
        line_text(&ov.tab_bar()).contains("0 Pipeline"),
        "the bar must print the digit: {}",
        line_text(&ov.tab_bar())
    );
}

#[test]
fn the_active_tab_is_never_clipped_out_of_the_bar() {
    // Ten labels plus digits and separators is ~100 cells; the box interior on
    // an 80-column terminal is 64. `Line::Split` used to cut the tail, which
    // silently ate the tab the user was standing on.
    let (mut ov, model, hist) = open_on(Rect::full(80, 24), full_snap());
    assert_eq!(ov.tabs.len(), MonitorTab::ALL.len());
    for want in MonitorTab::ALL {
        assert!(ov.goto_tab(want, &model, &ctx_at(&hist, Rect::full(80, 24))));
        let bar = line_text(&ov.tab_bar());
        assert!(
            bar.contains(want.label()),
            "{:?} is the active tab but is not in the bar: {bar}",
            want
        );
        // The windowing must also stay inside the width the split arm leaves —
        // otherwise `draw_line` is back to truncating and the guarantee is fake.
        assert!(
            crate::seg::cells(&bar) <= ov.cols,
            "bar overflows {} cells: {bar}",
            ov.cols
        );
    }
}

// --- Chunk 2: the org chart, the caps ladder, honest empty states ---------

/// Every heading in the built body, as `(label, note)`.
fn headings(ov: &MonitorOverlay) -> Vec<(String, String)> {
    ov.body
        .iter()
        .filter_map(|s| match s {
            Section::Heading { label, note } => {
                Some((label.clone(), note.clone().unwrap_or_default()))
            }
            _ => None,
        })
        .collect()
}

/// The board's stage headings — everything under the tab's own top line.
/// `spacer()` is itself an empty heading, so blanks are dropped too.
fn stage_headings(ov: &MonitorOverlay) -> Vec<(String, String)> {
    headings(ov)
        .into_iter()
        .filter(|(label, _)| !label.is_empty() && label != "agent pipeline")
        .collect()
}

/// One running roster row filed under `stage`.
fn board_row(id: i64, stage: &str, name: &str) -> thegn_core::issue::AgentDispatch {
    thegn_core::issue::AgentDispatch {
        id,
        issue_id: format!("THE-{id}"),
        worktree_path: format!("/wt/{name}"),
        agent_name: name.into(),
        dispatched_at_ms: 60_000,
        status: thegn_core::issue::AgentDispatchStatus::Running,
        stage: Some(stage.into()),
        parent_id: None,
        session_id: None,
        artifact_path: None,
    }
}

/// A board with a three-stage org chart and whatever rows are handed in.
fn model_with_org_chart(rows: Vec<thegn_core::issue::AgentDispatch>) -> FrameModel {
    let mut m = model_with(full_snap());
    m.dispatches = crate::monitor_pipeline::DispatchRoster {
        rows,
        stages: vec![
            stage("architect", Some("code")),
            stage("code", Some("review")),
            stage("review", None),
        ],
    };
    m
}

#[test]
fn a_configured_stage_with_no_rows_still_appears_on_the_board() {
    // A Lead must be able to see that `review` exists and is idle. The board is
    // the org chart, so an empty column is a fact, not an absence.
    let model = model_with_org_chart(vec![board_row(1, "code", "coder-a")]);
    let hist = history(120, NOW_MS);
    let ov = open_tab(MonitorTab::Pipeline, &model, &hist, Rect::full(120, 40));
    let heads = stage_headings(&ov);
    assert_eq!(
        heads.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        vec!["architect", "code", "review"],
        "configured order, every stage present"
    );
    assert!(heads[0].1.starts_with("idle"), "architect: {:?}", heads[0]);
    assert!(
        heads[1].1.starts_with("1 of 1 active"),
        "code: {:?}",
        heads[1]
    );
    assert!(heads[2].1.starts_with("idle"), "review: {:?}", heads[2]);
    // An idle stage carries no table — one line, not an empty grid.
    let tables = ov
        .body
        .iter()
        .filter(|s| matches!(s, Section::Table(_)))
        .count();
    assert_eq!(tables, 1, "only the staffed stage draws a table");
}

#[test]
fn a_stage_heading_carries_its_agent_concurrency_and_next() {
    let model = model_with_org_chart(vec![board_row(1, "code", "coder-a")]);
    let hist = history(120, NOW_MS);
    let ov = open_tab(MonitorTab::Pipeline, &model, &hist, Rect::full(120, 40));
    let heads = stage_headings(&ov);
    let chevron = crate::caps::glyph(thegn_core::termcaps::Glyph::Chevron);
    let code = &heads[1].1;
    assert!(code.contains("code-agent"), "no agent: {code}");
    assert!(code.contains("max 2"), "no concurrency: {code}");
    assert!(
        code.contains(&format!("{chevron} review")),
        "no hand-off: {code}"
    );
    // A terminal stage has nowhere to hand off to and must not pretend it does.
    let review = &heads[2].1;
    assert!(review.contains("review-agent") && review.contains("max 2"));
    assert!(
        !review.contains(chevron),
        "terminal stage points on: {review}"
    );
}

#[test]
fn an_empty_roster_with_a_configured_pipeline_draws_the_org_chart() {
    // `is_present` already shows this tab for a never-run pipeline; the board
    // must then show the chart rather than claiming there is nothing.
    let model = model_with_org_chart(vec![]);
    let hist = history(120, NOW_MS);
    let ov = open_tab(MonitorTab::Pipeline, &model, &hist, Rect::full(120, 40));
    let heads = stage_headings(&ov);
    assert_eq!(
        heads.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        vec!["architect", "code", "review"]
    );
    assert!(
        !heads.iter().any(|(l, _)| l == "no dispatches yet"),
        "an org chart is not nothing: {heads:?}"
    );
    // The tab's own top line still reports the (empty) roster honestly.
    assert_eq!(headings(&ov)[0].1, "0 rows · 0 active");
    // A roster with rows but no configured stages is the other direction: the
    // discovered stage still gets its group, with no org-chart numbers to quote.
    let mut discovered = model_with(full_snap());
    discovered.dispatches = crate::monitor_pipeline::DispatchRoster {
        rows: vec![board_row(1, "hotfix", "hand-run")],
        stages: vec![],
    };
    let ov = open_tab(
        MonitorTab::Pipeline,
        &discovered,
        &hist,
        Rect::full(120, 40),
    );
    assert_eq!(
        stage_headings(&ov),
        vec![("hotfix".to_string(), "1 of 1 active".to_string())]
    );
}

#[test]
fn board_row_glyphs_degrade_to_ascii() {
    use thegn_core::issue::AgentDispatchStatus as St;
    use thegn_core::termcaps::{UnicodeLevel, glyphs};
    let active = [
        St::Queued,
        St::Spawning,
        St::Running,
        St::WaitingHuman,
        St::PrOpen,
    ];
    let mut m = model_with(full_snap());
    m.dispatches = crate::monitor_pipeline::DispatchRoster {
        rows: active
            .iter()
            .enumerate()
            .map(|(i, &st)| {
                let mut r = board_row(i as i64 + 1, "code", &format!("a{i}"));
                r.status = st;
                r
            })
            .collect(),
        stages: vec![stage("code", None)],
    };
    let hist = history(120, NOW_MS);
    // The status glyph is resolved at the DRAW site, so a board built under the
    // ASCII rung carries ASCII — a `&'static str` frozen on the row could not.
    let ov = crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        open_tab(MonitorTab::Pipeline, &m, &hist, Rect::full(120, 40))
    });
    let table = cursor_table(&ov);
    assert_eq!(table.rows.len(), active.len());
    let mut seen: Vec<String> = Vec::new();
    for (row, st) in table.rows.iter().zip(active) {
        let crate::sections::Cell::Text(text, _) = &row[0] else {
            panic!("status cell is not text");
        };
        let want = st.glyph_token().resolve(glyphs(UnicodeLevel::Ascii));
        assert!(
            text.starts_with(&format!("{want} ")),
            "{st:?} did not degrade: {text:?}"
        );
        assert!(text.is_ascii(), "{st:?} rendered non-ASCII: {text:?}");
        assert!(
            !seen.contains(&want.to_string()),
            "{st:?} collides with an earlier active status at the ASCII rung"
        );
        seen.push(want.to_string());
    }
}

#[test]
fn the_containers_heading_does_not_claim_foreign_rows() {
    // The list explicitly includes containers thegn does not own, so the
    // heading may not call the whole table "thegn containers".
    let mut model = model_with(full_snap());
    model.containers.push(thegn_core::sandbox::ContainerInfo {
        name: "someone-elses".into(),
        image: "nginx".into(),
        status: "Up 3 hours".into(),
        ours: false,
        backend: "docker".into(),
        cpu: "0.1%".into(),
        mem: "9MiB".into(),
        net: "0B / 0B".into(),
        containment: String::new(),
        mounts: String::new(),
    });
    let hist = history(120, NOW_MS);
    let ov = open_tab(MonitorTab::Containers, &model, &hist, Rect::full(120, 40));
    let (label, note) = headings(&ov).remove(0);
    assert_eq!(label, "containers");
    assert!(note.contains("1 owned"), "owned count missing: {note}");
    assert!(note.contains("1 foreign"), "foreign count missing: {note}");
}

#[test]
fn an_unsampled_processes_tab_says_sampling_not_disabled() {
    // `ProcSnapshot::default()` is `enabled: false`, so the tab used to tell
    // every user whose first sample had not landed that their config said
    // something it did not.
    let hist = history(120, NOW_MS);
    let mut model = model_with(full_snap());
    assert!(model.procs_enabled() && !model.procs.enabled, "the fixture");
    let ov = open_tab(MonitorTab::Procs, &model, &hist, Rect::full(120, 40));
    let heads = headings(&ov);
    assert_eq!(heads.len(), 1);
    assert!(heads[0].0.starts_with("sampling"), "{heads:?}");

    // Only the CONFIG may claim sampling is off.
    model.procs_disabled = true;
    let ov = open_tab(MonitorTab::Procs, &model, &hist, Rect::full(120, 40));
    let heads = headings(&ov);
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].0.contains("[monitor] processes = false"),
        "{heads:?}"
    );
}
