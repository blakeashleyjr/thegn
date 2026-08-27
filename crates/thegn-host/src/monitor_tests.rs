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
            stage_order: vec!["code".into()],
        },
        ..Default::default()
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
