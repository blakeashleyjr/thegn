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
        net_ifaces: vec![thegn_metrics::NetIface {
            name: "eth0".into(),
            rx_bps: 5_000,
            tx_bps: 900,
            kind: thegn_metrics::IfaceKind::Ethernet,
            up: Some(true),
            physical: true,
        }],
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
            extra_mounts: 0,
        }]
        .into(),
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
        // `[monitor] processes` is on by default; `FrameModel` derives its
        // default, so the fixture states it exactly as `build_model` does.
        procs_disabled: false,
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
    assert_eq!(ov.prefs().last_tab, MonitorTab::Overview);

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
    assert_eq!(ch(&mut ov, '1'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.tab, MonitorTab::Overview);
    let tabs = ov.tabs.clone();
    assert_eq!(
        tabs.len(),
        MonitorTab::ALL.len() - 1,
        "full snapshot: all metric tabs"
    );
    let start = tabs.iter().position(|tab| *tab == ov.tab).unwrap();
    for expect in tabs.iter().cycle().skip(start + 1).take(tabs.len()) {
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
    let visible = MonitorTab::visible(&bare, false);
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
    assert!(MonitorTab::visible(&with_gpu, false).contains(&MonitorTab::Gpu));
    // Containers is hidden with no containers, present with at least one — the
    // "no engine, no tab" spec scenario.
    assert!(!MonitorTab::visible(&bare, false).contains(&MonitorTab::Containers));
    assert!(MonitorTab::visible(&bare, true).contains(&MonitorTab::Containers));
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
    assert_ne!(ov.tab, MonitorTab::ALL[3], "indexed ALL, not visible");
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
fn containers_tab_is_kept_until_listed_then_hidden_when_empty() {
    let hist = history(10, NOW_MS);
    let ctx = ctx_at(&hist, Rect::full(120, 40));
    let mut model = model_with(full_snap());
    model.containers.clear();
    // Never listed (the listing is demand-gated and arrives after open): a
    // persisted Containers tab is honoured rather than dropped to the first tab.
    model.containers_listed = false;
    let mut ov = MonitorOverlay::open(
        MonitorTab::Containers,
        MonitorPrefs::default(),
        &model,
        &ctx,
    );
    assert_eq!(ov.tab, MonitorTab::Containers);
    // Listed and genuinely empty: the tab goes away like any absent device.
    model.containers_listed = true;
    ov.refresh(&model, &ctx);
    assert_ne!(ov.tab, MonitorTab::Containers);
    assert!(!ov.tabs.contains(&MonitorTab::Containers));
    // Listed with a container: opens on it.
    model.containers = vec![thegn_core::sandbox::ContainerInfo {
        name: "thegn-x".into(),
        image: "i".into(),
        status: "Up".into(),
        ours: true,
        backend: "docker".into(),
        cpu: String::new(),
        mem: String::new(),
        net: String::new(),
        containment: String::new(),
        mounts: String::new(),
    }];
    let ov = MonitorOverlay::open(
        MonitorTab::Containers,
        MonitorPrefs::default(),
        &model,
        &ctx,
    );
    assert_eq!(ov.tab, MonitorTab::Containers);
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
    assert_eq!(
        ov.prefs.tab(MonitorTab::Cpu).window,
        Window::from_secs(86_400)
    );
    ch(&mut ov, ']');
    assert_eq!(
        ov.prefs.tab(MonitorTab::Cpu).window,
        Window::from_secs(86_400)
    );
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
fn only_the_containers_tab_shows_containers() {
    let model = model_with(full_snap());
    let hist = history(120, NOW_MS);
    let screen = Rect::full(120, 40);
    let mut ov = open_tab(MonitorTab::Cpu, &model, &hist, screen);
    assert!(!ov.shows_containers());

    ov = open_tab(MonitorTab::Containers, &model, &hist, screen);
    assert!(ov.shows_containers());
    assert_eq!(ch(&mut ov, ' '), MonitorOutcome::PrefsChanged);
    assert!(ov.is_paused());
    assert!(ov.shows_containers());
}

#[test]
fn a_wide_window_does_not_ask_for_fast_sampling() {
    // 500ms resolution buys nothing on an hour-wide plot, so it shouldn't cost
    // the wake rate either. Expressed against the ladder rather than a press
    // count, so adding a rung can't silently turn this into a no-op.
    let (mut ov, _m, _h) = open();
    assert!(ov.wants_live(), "the default window should sample live");
    while ov.prefs.tab(MonitorTab::Cpu).window != Window::from_secs(86_400) {
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

#[test]
fn process_sort_keys_render_both_directions_and_keep_confirmation_identity() {
    let screen = Rect::full(120, 40);
    let model = model_with_procs(vec![
        proc(42, None, "zulu", 20.0, 200),
        proc(3, None, "alpha", 30.0, 300),
        proc(17, None, "middle", 10.0, 100),
    ]);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, screen);

    for (key, sort, label, descending, ascending) in [
        ('c', ProcSort::Cpu, "cpu", [3, 42, 17], [17, 42, 3]),
        ('m', ProcSort::Rss, "mem", [3, 42, 17], [17, 42, 3]),
        ('n', ProcSort::Name, "name", [42, 17, 3], [3, 17, 42]),
        ('p', ProcSort::Pid, "pid", [42, 17, 3], [3, 17, 42]),
    ] {
        let starts_desc = ov.prefs.proc_desc;
        assert_eq!(ch(&mut ov, key), MonitorOutcome::PrefsChanged);
        ov.sync(&model, &hist, screen);
        assert_eq!(ov.prefs.proc_sort, sort);
        assert_eq!(ov.prefs.proc_desc, starts_desc);
        assert_eq!(
            ov.proc_rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
            if starts_desc { descending } else { ascending }
        );
        assert!(headings(&ov)[0].1.contains(&format!(
            "{label}{}",
            if ov.prefs.proc_desc { "↓" } else { "↑" }
        )));

        assert_eq!(ch(&mut ov, 'r'), MonitorOutcome::PrefsChanged);
        ov.sync(&model, &hist, screen);
        assert_eq!(ov.prefs.proc_desc, !starts_desc);
        assert_eq!(
            ov.proc_rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
            if !starts_desc { descending } else { ascending }
        );
        assert!(headings(&ov)[0].1.contains(&format!(
            "{label}{}",
            if ov.prefs.proc_desc { "↓" } else { "↑" }
        )));
    }

    // Sorting is complete before opening confirmation. A passive rank change
    // must leave the pending prompt bound to the sampled identity.
    ch(&mut ov, 'm');
    ch(&mut ov, 'r');
    ov.sync(&model, &hist, screen);
    assert!(!ov.prefs.proc_desc);
    assert_eq!(ov.proc_rows[ov.sel].pid, 17);
    ch(&mut ov, 'x');
    let (pid, start_time, label) = match &ov.confirm {
        Some(super::Confirm::Signal {
            pid,
            start_time,
            label,
            ..
        }) => (*pid, *start_time, label.clone()),
        other => panic!("expected signal confirmation, got {other:?}"),
    };
    assert_eq!(
        (pid, start_time, label.as_str()),
        (17, 100, "pid 17 middle")
    );
    let mut changed = model.clone();
    changed.procs.procs[2].rss_bytes = 400;
    changed.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&changed, &ctx_at(&hist, screen));
    assert!(!ov.prefs.proc_desc);
    assert_eq!(
        ov.proc_rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
        [42, 3, 17]
    );
    assert!(matches!(
        &ov.confirm,
        Some(super::Confirm::Signal {
            pid: current_pid,
            start_time: current_start,
            label: current_label,
            ..
        }) if (*current_pid, *current_start, current_label) == (pid, start_time, &label)
    ));
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
    // The CPU board renders per-core readings from the sampled monitor frame.
    let (mut ov, _m, _h) = open();
    let mut frame = thegn_metrics::collect::frame::MonitorFrame::default();
    frame.health[thegn_metrics::collect::CollectorId::CpuStat.index()] =
        thegn_metrics::collect::health::Health::Ok;
    frame.health[thegn_metrics::collect::CollectorId::CpuTopology.index()] =
        thegn_metrics::collect::health::Health::Ok;
    frame
        .parts
        .cpu_stat
        .cores
        .push(thegn_metrics::collect::model::cpu_stat::CoreSample {
            id: 0,
            online: true,
            share: Some(thegn_metrics::collect::model::cpu_stat::ModeShare {
                busy: 500,
                ..Default::default()
            }),
        });
    frame.parts.cpu_topology.cpus.push(
        thegn_metrics::collect::model::cpu_topology::CpuTopologyEntry {
            id: 0,
            core_type: thegn_metrics::collect::policy::cpu_topology::CoreType::Performance,
            online: true,
            ..Default::default()
        },
    );
    frame.parts.cpu_topology.l3_groups.push(vec![0]);
    assert!(ov.ingest_frame(&frame));
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("0 P L30"), "per-core reading missing: {text}");
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
        start_time: 100,
        start_ticks: None,
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
        summary: None,
    };
    m.process_state = crate::model_eq::ProcessViewState::Fresh;
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
            Section::Table(t) | Section::FixedTable { table: t, .. } if t.sel.is_some() => Some(t),
            _ => None,
        })
        .expect("a table with a row cursor")
}

/// How many tables in the built body claim a cursor.
fn cursor_tables(ov: &MonitorOverlay) -> usize {
    ov.body
        .iter()
        .filter(|s| matches!(s, Section::Table(t) | Section::FixedTable { table: t, .. } if t.sel.is_some()))
        .count()
}

#[test]
fn process_columns_follow_viewport_not_sample_values_and_keep_visible_identity() {
    let hist = TelemetryHistory::default();
    for cols in [26, 40, 80, 160, 240] {
        let screen = Rect::full(cols, 30);
        let mut previous_widths = None;
        for changed in [false, true] {
            let mut sample = proc(
                4_194_305,
                None,
                if changed {
                    "fixture-世-e\u{301}-👩‍💻-longer-name"
                } else {
                    "fixture"
                },
                if changed { 1200.5 } else { 0.1 },
                if changed { u64::MAX } else { 1 },
            );
            sample.owner = if changed {
                thegn_metrics::ProcOwner::Pane(123456)
            } else {
                thegn_metrics::ProcOwner::Other
            };
            let model = model_with_procs(vec![sample]);
            let mut ov = MonitorOverlay::open(
                MonitorTab::Procs,
                MonitorPrefs::default(),
                &model,
                &ctx_at(&hist, screen),
            );
            let (table, widths) = ov
                .body
                .iter()
                .find_map(|section| match section {
                    Section::FixedTable { table, widths } => Some((table, widths)),
                    _ => None,
                })
                .expect("Processes opts into fixed columns");
            assert_eq!(table.rows.len(), 1);
            assert_eq!(
                widths.iter().sum::<usize>() + widths.len().saturating_sub(1) + 1,
                ov.cols
            );
            if cols >= 80 {
                assert_eq!(
                    table.header.iter().map(String::as_str).collect::<Vec<_>>(),
                    ["PID", "OWNER", "CPU%", "usr/sys", "RSS", "COMMAND"]
                );
            }
            assert!(matches!(
                &table.rows[0][0],
                crate::sections::Cell::Text(value, _) if value == "4194305"
            ));
            if let Some(previous) = &previous_widths {
                assert_eq!(previous, widths);
            }
            previous_widths = Some(widths.clone());
            let mut surface = Surface::new(cols, 30);
            ov.render(&mut surface, screen);
            if cols >= 40 {
                let text: String = surface
                    .screen_lines()
                    .iter()
                    .flat_map(|line| {
                        line.visible_cells()
                            .map(|cell| cell.str().to_owned())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                assert!(text.contains("fixture"));
                assert!(
                    text.contains("4194305"),
                    "full seven-digit PID is visible: {text}"
                );
            }
            assert_eq!(ch(&mut ov, 'x'), MonitorOutcome::Pending);
            assert!(matches!(
                ov.confirm,
                Some(Confirm::Signal {
                    pid: 4_194_305,
                    start_time: 100,
                    ..
                })
            ));
            // Inspect confirmation only; never dispatch a process signal.
        }
    }
}

#[test]
fn process_columns_use_available_fitted_default_and_keep_cpu_unprimed_unknown() {
    let hist = history(120, NOW_MS);
    for cols in [100usize, 160, 240] {
        let screen = Rect::full(cols, 30);
        let mut model = model_with_procs(vec![proc(41, None, "worker", 0.0, 4096)]);
        model.procs.primed = false;
        let ov = open_tab(MonitorTab::Procs, &model, &hist, screen);
        let table = ov
            .body
            .iter()
            .find_map(|section| match section {
                Section::FixedTable { table, .. } => Some(table),
                _ => None,
            })
            .expect("process table is rendered");
        assert_eq!(
            table.header.iter().map(String::as_str).collect::<Vec<_>>(),
            ["PID", "OWNER", "CPU%", "usr/sys", "RSS", "COMMAND"]
        );
        let cpu = table
            .header
            .iter()
            .position(|header| header == "CPU%")
            .unwrap();
        assert!(matches!(
            &table.rows[0][cpu],
            crate::sections::Cell::Text(value, _) if value.trim() == "—"
        ));
        let cpu_bar = table
            .header
            .iter()
            .position(|header| header == "usr/sys")
            .unwrap();
        assert!(matches!(
            &table.rows[0][cpu_bar],
            crate::sections::Cell::Text(value, _) if value.trim() == "—"
        ));
        assert_eq!(
            table.rows[0].len(),
            table.header.len(),
            "each fitted header has exactly one cell"
        );
    }
}

#[test]
fn process_filter_lists_ignored_terms_and_still_matches_valid_bare_terms() {
    let screen = Rect::full(120, 40);
    let model = model_with_procs(vec![
        proc(101, None, "kworker/0:1", 0.0, 1),
        proc(102, None, "bash", 0.0, 1),
    ]);
    let hist = history(120, NOW_MS);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, screen);
    ov.filter = "kworker/0:1 thr>abc user:".into();
    ov.sync(&model, &hist, screen);
    assert_eq!(
        ov.proc_rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
        [101]
    );
    assert!(ov.body.iter().any(|section| matches!(
        section,
        Section::HeadingToned { label, note, .. }
            if label == "ignored filter terms"
                && note == "thr>abc (bad value), user: (empty value)"
    )));
    ov.filtering = true;
    ov.sync(&model, &hist, screen);
    assert!(!ov.body.iter().any(|section| matches!(
        section,
        Section::HeadingToned { label, .. } if label == "ignored filter terms"
    )));
}

#[test]
fn process_columns_stay_inside_very_narrow_viewports() {
    let hist = history(10, NOW_MS);
    let model = model_with_procs(vec![proc(41, None, "worker", 0.0, 4096)]);
    for cols in 1..=6 {
        let screen = Rect::full(cols, 12);
        let ov = open_tab(MonitorTab::Procs, &model, &hist, screen);
        let widths = ov.body.iter().find_map(|section| match section {
            Section::FixedTable { widths, .. } => Some(widths),
            _ => None,
        });
        if let Some(widths) = widths {
            assert_eq!(
                widths.iter().sum::<usize>() + widths.len().saturating_sub(1) + 1,
                ov.cols
            );
        }
    }
}

#[test]
fn process_reflow_keeps_selected_identity_and_fits_the_resized_viewport() {
    let hist = history(120, NOW_MS);
    let model = model_with_n_procs(12);
    let small = Rect::full(100, 30);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, small);
    for _ in 0..3 {
        press(&mut ov, &model, &hist, small, 'j');
    }
    let selected_pid = ov.proc_rows[ov.sel].pid;
    let wide = Rect::full(240, 30);
    assert!(ov.reflow_geometry(wide));
    assert_eq!(ov.proc_rows[ov.sel].pid, selected_pid);
    let (table, widths) = ov
        .body
        .iter()
        .find_map(|section| match section {
            Section::FixedTable { table, widths } => Some((table, widths)),
            _ => None,
        })
        .expect("process table remains after reflow");
    assert_eq!(
        widths.iter().sum::<usize>() + widths.len().saturating_sub(1) + 1,
        ov.cols
    );
    assert_eq!(
        table.header.iter().map(String::as_str).collect::<Vec<_>>(),
        ["PID", "OWNER", "CPU%", "usr/sys", "RSS", "COMMAND"]
    );
    assert_eq!(table.sel, Some(3));
    ch(&mut ov, 'x');
    assert!(matches!(ov.confirm, Some(Confirm::Signal { pid, .. }) if pid == selected_pid));
}

#[test]
fn process_reflow_uses_frozen_priming_state_when_filter_mentions_warming_note() {
    let hist = history(120, NOW_MS);
    let mut model = model_with_procs(vec![proc(41, None, "cpu warming up worker", 12.0, 4096)]);
    model.procs.primed = true;
    let small = Rect::full(100, 30);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, small);
    ov.filter = "cpu warming up".into();
    ov.sync(&model, &hist, small);
    assert_eq!(ov.proc_rows.len(), 1);
    let wide = Rect::full(160, 30);
    assert!(ov.reflow_geometry(wide));
    let cpu = ov
        .body
        .iter()
        .find_map(|section| match section {
            Section::FixedTable { table, .. } => table
                .header
                .iter()
                .position(|header| header == "CPU%")
                .map(|index| &table.rows[0][index]),
            _ => None,
        })
        .expect("CPU% cell is rendered");
    assert!(matches!(cpu, crate::sections::Cell::Text(value, _) if value.trim() != "—"));
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
fn the_last_tab_is_reachable_by_its_digit() {
    // Every family on this fixture is visible, so the LAST tab is the exact
    // case where the ten visible digit keys cover the tab list, including the
    // special zero key for the tenth tab.
    let (mut ov, _m, _h) = open();
    assert_eq!(ov.tabs.len(), MonitorTab::ALL.len() - 1);
    let last = *ov.tabs.last().expect("at least one tab");
    let d = crate::monitor::tabbar::digit(ov.tabs.len() - 1).expect("a digit");
    assert_eq!(ch(&mut ov, d), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.tab, last);
    // …and the bar says so, rather than making the user guess.
    assert!(
        line_text(&ov.tab_bar()).contains(&format!("{d} {}", last.label())),
        "the bar must print the digit: {}",
        line_text(&ov.tab_bar())
    );
}

#[test]
fn zero_selects_tenth_tab_and_eleventh_remains_keyboard_reachable() {
    // Zero names the tenth visible tab. Notifications is eleventh and remains
    // reachable by cycling because there is no eleventh digit.
    let (mut ov, _m, _h) = open();
    assert_eq!(ov.tabs.len(), 10);
    assert_eq!(ch(&mut ov, '0'), MonitorOutcome::PrefsChanged);
    assert_eq!(ov.tab, ov.tabs[9]);
    ov.tabs = MonitorTab::visible_for(&full_snap(), true, true);
    assert_eq!(ov.tabs.last(), Some(&MonitorTab::Notifications));
    assert_eq!(crate::monitor::tabbar::digit(10), None);
    while ov.tab != MonitorTab::Notifications {
        key(&mut ov, KeyCode::Tab);
    }
    assert_eq!(ov.tab, MonitorTab::Notifications);
}

#[test]
fn the_active_tab_is_never_clipped_out_of_the_bar() {
    // Ten labels plus digits and separators is ~100 cells; the box interior on
    // an 80-column terminal is 64. `Line::Split` used to cut the tail, which
    // silently ate the tab the user was standing on. Walk the tabs the way a
    // user does — the digit keys — and stand on every one of them.
    let (mut ov, _m, _h) = open_on(Rect::full(80, 24), full_snap());
    assert_eq!(ov.tabs.len(), MonitorTab::ALL.len() - 1);
    let tabs = ov.tabs.clone();
    for (i, want) in tabs.iter().enumerate() {
        assert_eq!(
            ch(
                &mut ov,
                crate::monitor::tabbar::digit(i).expect("a visible tab has a digit")
            ),
            MonitorOutcome::PrefsChanged,
            "digit {i} did not move"
        );
        assert_eq!(ov.tab, *want);
        let bar = line_text(&ov.tab_bar());
        assert!(
            bar.contains(want.label()),
            "{want:?} is the active tab but is not in the bar: {bar}"
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

#[test]
fn failed_process_sampling_replaces_old_rows_with_an_explicit_error() {
    let hist = history(120, NOW_MS);
    let mut model = model_with_procs(vec![proc(1000, None, "old-worker", 2.0, 1024)]);
    let mut ov = open_tab(MonitorTab::Procs, &model, &hist, Rect::full(120, 40));
    assert!(render_text(&ov, 120, 40).contains("old-worker"));

    // Keep the publication revision unchanged: state is the invalidation key.
    model.fail_processes("sampler stopped");
    assert!(ov.refresh(&model, &ctx_at(&hist, Rect::full(120, 40))));
    let text = render_text(&ov, 120, 40);
    assert!(text.contains("process sampling unavailable: sampler stopped"));
    assert!(!text.contains("old-worker"));
}

// --- Chunk 3: per-tab footer hints and the help door ---------------------

/// The footer text for `tab`, built straight from the pure builder — no
/// overlay, which is the point of [`footer::FooterInput`].
fn footer_for(tab: MonitorTab) -> String {
    let prefs = MonitorPrefs::default();
    line_text(&footer::line(footer::FooterInput {
        tab,
        prefs: &prefs,
        confirm: None,
        filtering: false,
        filter: "",
        notice: None,
        status: None,
        paused: false,
        // Generous inputs: the hints that DO depend on state are all switched
        // on, so an assertion about an absent hint can only be about the tab.
        container_ours: true,
        disk_rows: 3,
        board_panels: 1,
        board_zoomed: false,
    }))
}

/// The three graph-toggle hints, as the footer spells them for `tab`.
fn graph_hints(tab: MonitorTab) -> [String; 3] {
    let prefs = MonitorPrefs::default();
    let p = prefs.tab(tab);
    [
        "[ ]".to_string(),
        p.style.label().to_string(),
        p.scale.label().to_string(),
    ]
}

#[test]
fn the_footer_only_advertises_keys_the_tab_has() {
    // Processes emits only headings and a table, so `[ ]` / `g` / `s` named
    // four keys with nothing on screen to act on.
    let procs = footer_for(MonitorTab::Procs);
    for hint in graph_hints(MonitorTab::Procs) {
        assert!(
            !procs.contains(&hint),
            "Processes footer still advertises `{hint}`: {procs}"
        );
    }
    // …and the keys it DOES have are still there.
    assert!(procs.contains("sort"), "{procs}");
    assert!(procs.contains("signal"), "{procs}");

    let cpu = footer_for(MonitorTab::Cpu);
    for hint in graph_hints(MonitorTab::Cpu) {
        assert!(cpu.contains(&hint), "CPU footer lost `{hint}`: {cpu}");
    }
}

#[test]
fn every_tab_advertises_pause() {
    // Including Containers and Disk: `Space` freezes whatever is in front of
    // you, and a footer that hides that is how a user ends up staring at a
    // stale picture.
    for tab in MonitorTab::ALL {
        let text = footer_for(tab);
        assert!(
            text.contains("pause"),
            "{tab:?} footer has no pause hint: {text}"
        );
    }
    // And it flips to `resume` while frozen.
    let prefs = MonitorPrefs::default();
    let frozen = line_text(&footer::line(footer::FooterInput {
        tab: MonitorTab::Disk,
        prefs: &prefs,
        confirm: None,
        filtering: false,
        filter: "",
        notice: None,
        status: None,
        paused: true,
        container_ours: false,
        disk_rows: 3,
        board_panels: 1,
        board_zoomed: false,
    }));
    assert!(frozen.contains("resume"), "{frozen}");
}

#[test]
fn the_footer_advertises_help_on_every_tab() {
    for tab in MonitorTab::ALL {
        let text = footer_for(tab);
        assert!(
            text.contains("help"),
            "{tab:?} footer has no help hint: {text}"
        );
        // Immediately before the right-hand slot, so it reads as the last
        // resort rather than as one hint among the tab's own actions.
        let help = text.find("help").expect("help hint");
        let close = text.find("q close").expect("close hint");
        assert!(help < close, "{tab:?}: help must precede the close slot");
    }
}

#[test]
fn question_mark_and_f1_ask_for_help() {
    // A graph tab, where nothing competes for the key…
    let (mut ov, _m, _h) = open();
    assert_eq!(ch(&mut ov, '?'), MonitorOutcome::Help);
    assert_eq!(key(&mut ov, KeyCode::Function(1)), MonitorOutcome::Help);

    // …and Processes, whose per-tab letter arm must not shadow it. (F1 used to
    // match no arm at all and fall to `Pending`, so the modal ate the global
    // help key everywhere.)
    goto_procs(&mut ov);
    assert_eq!(ch(&mut ov, '?'), MonitorOutcome::Help);
    assert_eq!(key(&mut ov, KeyCode::Function(1)), MonitorOutcome::Help);

    // But a `?` TYPED INTO the filter is text, not a request for help: the
    // sub-mode returns above the global arms.
    ch(&mut ov, '/');
    assert_eq!(ch(&mut ov, '?'), MonitorOutcome::Pending);
    assert_eq!(ov.filter, "?");
}

#[test]
fn has_graphs_matches_what_the_builders_emit() {
    // Table-driven over every tab, so a new tab cannot silently inherit the
    // wrong footer: the gate must agree with whether the builder emits a plot.
    let mut model = model_with(full_snap());
    model.containers.push(thegn_core::sandbox::ContainerInfo {
        name: "thegn-wt".into(),
        image: "alpine".into(),
        status: "Up 1 hour".into(),
        ours: true,
        backend: "podman".into(),
        cpu: "1%".into(),
        mem: "10MiB".into(),
        net: "0B / 0B".into(),
        containment: String::new(),
        mounts: String::new(),
    });
    model.notification_delivery_configured = true;
    let hist = history(120, NOW_MS);
    for tab in MonitorTab::ALL {
        let ov = open_tab(tab, &model, &hist, Rect::full(120, 40));
        assert_eq!(ov.tab, tab, "{tab:?} was not present in the fixture");
        // Overview draws its history sparkline through the board painter,
        // rather than emitting a legacy Section::Graph.
        let plots =
            tab == MonitorTab::Overview || ov.body.iter().any(|s| matches!(s, Section::Graph(_)));
        assert_eq!(
            plots,
            tab.has_graphs(),
            "{tab:?}: has_graphs() = {}, but the builder emitted {} plot(s)",
            tab.has_graphs(),
            usize::from(plots)
        );
    }
}

// THE-628: a process snapshot belongs to its sampler, not model hydration.
#[test]
fn process_table_survives_interleaved_hydration_and_samples() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(40);
    m.process_revision = 41;
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.nav(20);
    ov.sync(&m, &h, screen);
    let selected = ov.proc_rows[ov.sel].pid;
    let scroll = ov.scroll();
    for pass in 0..4 {
        let mut hydrated = model_with(full_snap());
        assert!(hydrated.procs.procs.is_empty());
        hydrated.carry_monitor_state_from(&mut m);
        m = hydrated;
        ov.refresh(&m, &ctx_at(&h, screen));
        assert_eq!(m.process_revision, 41 + pass);
        assert_eq!(ov.proc_rows.len(), 40);
        assert_eq!(ov.proc_rows[ov.sel].pid, selected);
        assert_eq!(ov.scroll(), scroll);
        assert!(cursor_on_screen(&ov));
        assert!(!headings(&ov).iter().any(|(title, _)| title == "sampling…"));
        m.procs.procs[0].rss_bytes += pass;
        m.process_revision += 1;
        ov.refresh(&m, &ctx_at(&h, screen));
    }
}

#[test]
fn passive_process_refresh_follows_identity_after_rank_changes() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(40);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.nav(20);
    ov.sync(&m, &h, screen);
    let identity = (ov.proc_rows[ov.sel].pid, ov.proc_rows[ov.sel].start_time);
    m.procs.procs[20].cpu_pct = 1000.0;
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(ov.sel, 0);
    assert_eq!(
        (ov.proc_rows[ov.sel].pid, ov.proc_rows[ov.sel].start_time),
        identity
    );
    assert!(cursor_on_screen(&ov));
    ov.begin_signal();
    match &ov.confirm {
        Some(super::Confirm::Signal {
            pid, start_time, ..
        }) => assert_eq!((*pid, *start_time), identity),
        _ => panic!("expected confirmation for selected identity"),
    }
}

#[test]
fn process_refresh_does_not_yank_a_manually_scrolled_viewport() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(40);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.wheel(15);
    let scroll = ov.scroll();
    m.procs.procs[0].cpu_pct = -1.0;
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(ov.sel, 39);
    assert_eq!(ov.scroll(), scroll);
    assert!(!ov.follow);
}

#[test]
fn process_confirmation_refuses_a_reused_or_disappeared_sampled_identity() {
    for reused in [false, true] {
        let h = TelemetryHistory::default();
        let screen = Rect::full(100, 24);
        let mut m = model_with_n_procs(3);
        // PID 0 is rejected before any OS signal on every supported platform,
        // keeping this fixture safe even if the identity guard regresses.
        m.procs.procs[0].pid = 0;
        let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
        ov.begin_signal();
        if reused {
            m.procs.procs[0].start_time += 1;
        } else {
            m.procs.procs.remove(0);
        }
        m.process_revision += 1; // emulate the sampler publication boundary
        ov.refresh(&m, &ctx_at(&h, screen));
        // This must be the identity refusal, not the platform invalid-PID error.
        ov.confirm_key(&KeyCode::Char('y'));
        assert!(ov.confirm.is_none());
        assert!(
            ov.status
                .as_deref()
                .unwrap()
                .contains("process changed or left")
        );
        assert!(ov.last_termed.is_none());
    }
}

#[test]
fn reused_process_does_not_inherit_selection_or_signal_escalation() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(3);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.last_termed = Some((1000, 100));
    m.procs.procs[0].start_time = 200;
    m.procs.procs[0].cpu_pct = -1.0;
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(
        ov.proc_rows[ov.sel].pid, 1001,
        "fallback stays at old index rather than following reused PID"
    );
    ov.nav(2);
    ov.begin_signal();
    assert!(matches!(
        ov.confirm,
        Some(super::Confirm::Signal {
            pid: 1000,
            stage: crate::platform::ProcSignal::Terminate,
            ..
        })
    ));
}

#[test]
fn process_exit_clamps_and_explicit_sort_still_resets_selection() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(40);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.nav(39);
    ov.sync(&m, &h, screen);
    m.procs.procs.truncate(2);
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(ov.sel, 1);
    assert!(cursor_on_screen(&ov));
    ov.proc_key('m');
    ov.sync(&m, &h, screen);
    assert_eq!(ov.sel, 0);
    m.procs.procs.clear();
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(ov.sel, 0);
    assert_eq!(ov.scroll(), 0);
}

#[test]
fn paused_process_refresh_does_not_change_the_displayed_snapshot() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(3);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.paused = true;
    let before = ov.proc_rows.clone();
    m.procs.procs.reverse();
    m.procs.procs[0].cpu_pct = 1000.0;
    m.process_revision += 1;
    m.fail_processes("sampler stopped while frozen");
    assert!(!ov.refresh(&m, &ctx_at(&h, screen)));
    assert_eq!(ov.proc_rows, before);
}

#[test]
fn process_confirmation_keeps_its_identity_while_ranks_change() {
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(3);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ov.begin_signal();
    m.procs.procs[0].cpu_pct = -1.0;
    m.process_revision += 1; // emulate the sampler publication boundary
    ov.refresh(&m, &ctx_at(&h, screen));
    assert_eq!(ov.sel, 2);
    assert!(matches!(
        ov.confirm,
        Some(super::Confirm::Signal {
            pid: 1000,
            start_time: 100,
            ..
        })
    ));
    // Do not confirm: the fixture validates the target, never sends a signal.
}

#[test]
fn disk_volume_row_displays_collapsed_mount_count() {
    let mut stats = full_snap();
    let mut disks = stats.disks.to_vec();
    disks[0].extra_mounts = 42;
    disks[0].mount = "/var/lib/containers/storage".into();
    stats.disks = disks.into();
    let model = model_with(stats);
    let hist = history(120, NOW_MS);
    let overlay = open_tab(MonitorTab::Disk, &model, &hist, SHORT);
    assert!(render_text(&overlay, 100, 30).contains("+42"));
}

#[test]
fn memory_tab_wires_board_demand_and_freezes_frames() {
    let hist = TelemetryHistory::default();
    let screen = Rect::full(160, 60);
    let stats = StatsSnapshot {
        mem_gib: Some((8.0, 32.0)),
        mem_free_gib: Some(3.0),
        ..full_snap()
    };
    let model = model_with(stats);
    let mut ov = open_tab(MonitorTab::Memory, &model, &hist, screen);
    let placed = ov.placed_panels();
    assert!(placed.iter().any(|panel| panel.id.0.starts_with("memory.")));
    assert!(!placed.iter().any(|panel| panel.id.0 == "legacy"));
    assert!(placed.iter().any(|panel| panel.id.0 == "memory.limit"));
    let demand = {
        use super::board::TabView as _;
        let view = ov.view();
        let demand = ov.board.demand(&view, ov.body_rect(), 1000).collectors;
        let expected = placed.iter().fold(
            thegn_metrics::collect::CollectorSet::empty(),
            |mut set, panel| {
                for id in view.demand(panel.id).iter() {
                    set.insert(id);
                }
                set
            },
        );
        assert_eq!(demand, expected);
        demand
    };
    assert!(demand.contains(thegn_metrics::collect::CollectorId::CgroupSlice));
    assert!(!demand.contains(thegn_metrics::collect::CollectorId::CpuStat));

    let mut frame = thegn_metrics::collect::frame::MonitorFrame::default();
    thegn_metrics::collect::parse::meminfo::parse(
        b"MemTotal: 32768 kB\nMemAvailable: 24576 kB\nMemFree: 3072 kB\n",
        &mut frame.parts.meminfo.raw,
    );
    assert!(ov.ingest_frame(&frame));
    assert_eq!(ov.memory.total, Some(32 * 1024 * 1024));
    ov.paused = true;
    let mut next = thegn_metrics::collect::frame::MonitorFrame::default();
    thegn_metrics::collect::parse::meminfo::parse(
        b"MemTotal: 65536 kB\nMemAvailable: 32768 kB\n",
        &mut next.parts.meminfo.raw,
    );
    assert!(!ov.ingest_frame(&next));
    assert_eq!(ov.memory.total, Some(32 * 1024 * 1024));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reaps its harmless child process"
)]
fn process_confirmation_refuses_changed_live_process_identity() {
    use std::process::Command;

    struct FixtureChild(std::process::Child);

    impl Drop for FixtureChild {
        fn drop(&mut self) {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "could not check monitor fixture before cleanup")
                }
            }
            if let Err(error) = self.0.kill() {
                tracing::debug!(%error, "could not kill monitor fixture during cleanup");
            }
            if let Err(error) = self.0.wait() {
                tracing::debug!(%error, "could not reap monitor fixture during cleanup");
            }
        }
    }

    let mut child = FixtureChild(
        Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", "monitor::tests::identity_sleep_fixture"])
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("THEGN_SIGNAL_IDENTITY_FIXTURE", "1")
            .spawn()
            .expect("spawn identity fixture"),
    );
    let pid = child.0.id();
    let start_time = 1;
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(1);
    m.procs.procs[0].pid = pid;
    m.procs.procs[0].start_time = start_time;
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ch(&mut ov, 'x');
    ch(&mut ov, 'y');
    assert!(ov.status.as_deref().unwrap().contains("process changed"));
    assert!(ov.last_termed.is_none());
    assert!(child.0.try_wait().expect("check child status").is_none());
    child.0.kill().expect("kill fixture child after mismatch");
    child.0.wait().expect("reap fixture child after mismatch");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the fixture child is reaped on every path"
)]
fn process_confirmation_threads_sampled_ticks_to_the_signal() {
    use std::process::Command;

    struct FixtureChild(std::process::Child);

    impl Drop for FixtureChild {
        fn drop(&mut self) {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "could not check signal fixture before cleanup")
                }
            }
            if let Err(error) = self.0.kill() {
                tracing::debug!(%error, "could not kill signal fixture during cleanup");
            }
            if let Err(error) = self.0.wait() {
                tracing::debug!(%error, "could not reap signal fixture during cleanup");
            }
        }
    }

    let mut child = FixtureChild(
        Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", "monitor::tests::identity_sleep_fixture"])
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("THEGN_SIGNAL_IDENTITY_FIXTURE", "1")
            .spawn()
            .expect("spawn identity fixture"),
    );
    let pid = child.0.id();
    let Some(start_ticks) = crate::platform::proxy_process_start_time(pid) else {
        return;
    };
    let h = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let mut m = model_with_n_procs(1);
    m.procs.procs[0].pid = pid;
    m.procs.procs[0].start_time = 1;
    m.procs.procs[0].start_ticks = Some(start_ticks);
    let mut ov = open_tab(MonitorTab::Procs, &m, &h, screen);
    ch(&mut ov, 'x');
    ch(&mut ov, 'y');
    assert!(ov.status.as_deref().unwrap().contains("sent SIGTERM"));
    let status = child.0.wait().expect("reap signalled identity fixture");
    assert_eq!(
        status.code(),
        None,
        "SIGTERM should end the fixture by signal"
    );
}

#[test]
fn identity_sleep_fixture() {
    if std::env::var_os("THEGN_SIGNAL_IDENTITY_FIXTURE").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

#[path = "monitor_cache_tests.rs"]
mod invalidation_tests;

#[test]
fn network_tab_keeps_and_ghosts_idle_and_down_interfaces() {
    let stats = StatsSnapshot {
        net_bps: Some((250, 100)),
        net_bps_all: Some((400, 150)),
        net_ifaces: vec![
            thegn_metrics::NetIface {
                name: "enp95s0".into(),
                rx_bps: 0,
                tx_bps: 0,
                kind: thegn_metrics::IfaceKind::Ethernet,
                up: Some(false),
                physical: true,
            },
            thegn_metrics::NetIface {
                name: "wg0".into(),
                rx_bps: 0,
                tx_bps: 0,
                kind: thegn_metrics::IfaceKind::Wireguard,
                up: Some(true),
                physical: false,
            },
            thegn_metrics::NetIface {
                name: "wlan0".into(),
                rx_bps: 250,
                tx_bps: 100,
                kind: thegn_metrics::IfaceKind::Wifi,
                up: Some(true),
                physical: true,
            },
        ],
        ..Default::default()
    };
    let model = model_with(stats);
    let history = TelemetryHistory::default();
    let screen = Rect::full(100, 24);
    let overlay = open_tab(MonitorTab::Network, &model, &history, screen);
    let table = overlay
        .body
        .iter()
        .find_map(|section| match section {
            Section::Table(table) => Some(table),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        table.header.iter().map(String::as_str).collect::<Vec<_>>(),
        ["iface", "kind", "state", "rx", "tx"]
    );
    assert_eq!(table.rows.len(), 3);
    for row in &table.rows[..2] {
        assert!(
            row.iter()
                .all(|cell| cell_tone(cell) == Tok::Slot(S::Ghost))
        );
    }
    assert!(
        table.rows[2]
            .iter()
            .any(|cell| cell_tone(cell) != Tok::Slot(S::Ghost))
    );
    assert!(overlay.body.iter().any(|section| matches!(section,
        Section::Heading { note: Some(note), .. } if note == "3, 1 down"
    )));
    assert!(overlay.body.iter().any(|section| matches!(section,
        Section::KeyVal(values) if values.iter().any(|(key, _, _)| key == "all links")
    )));
}

#[test]
fn gpu_monitor_labels_shared_memory_and_preserves_vram_for_discrete_cards() {
    fn labels(shared: bool) -> Vec<String> {
        let hist = history(10, NOW_MS);
        let mut stats = full_snap();
        stats.gpu_mem_shared = shared;
        let model = model_with(stats);
        let tab = super::build::tab(super::build::TabInput {
            tab: MonitorTab::Gpu,
            model: &model,
            hist: &hist,
            prefs: TabPrefs::default(),
            cols: 120,
            now_ms: NOW_MS,
            sel: 0,
            filter: "",
            filtering: false,
            tree: false,
            proc_sort: ProcSort::default(),
            proc_desc: false,
            proc_rows: &[],
            proc_invalid_terms: &[],
            proc_primed: false,
            disk_rows: &[],
            disk_eta: None,
        });
        tab.sections
            .into_iter()
            .flat_map(|section| match section {
                crate::sections::Section::Grid { cells, .. } => cells
                    .into_iter()
                    .map(|(label, _, _)| label)
                    .collect::<Vec<_>>(),
                crate::sections::Section::Sparkrow { label, .. } => vec![label],
                _ => Vec::new(),
            })
            .collect()
    }

    let shared = labels(true);
    assert!(shared.contains(&"gpu mem (shared)".to_owned()));
    assert!(shared.contains(&"gpu mem used".to_owned()));
    assert!(shared.contains(&"gpu mem".to_owned()));
    assert!(
        !shared
            .iter()
            .any(|label| label == "vram" || label == "vram used")
    );
    let discrete = labels(false);
    assert!(discrete.contains(&"vram".to_owned()));
    assert!(discrete.contains(&"vram used".to_owned()));
}

#[test]
fn overview_is_the_first_default_tab_and_old_cpu_slug_restores() {
    assert_eq!(MonitorTab::ALL[0], MonitorTab::Overview);
    assert_eq!(MonitorTab::default(), MonitorTab::Overview);
    assert_eq!(MonitorTab::from_key("overview"), Some(MonitorTab::Overview));
    assert_eq!(MonitorTab::from_key("cpu"), Some(MonitorTab::Cpu));
    let (mut overlay, _model, _history) = open();
    ch(&mut overlay, '1');
    assert_eq!(overlay.tab(), MonitorTab::Overview);
}

#[test]
fn overview_ingest_is_visible_only_and_pause_freezes_its_copy() {
    let (mut overlay, _, _) = open();
    let frame = thegn_metrics::collect::frame::MonitorFrame {
        parts: thegn_metrics::collect::frame::Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                procs_running: Some(7),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.cpu.procs_running(), Some(7));
    overlay.tab = MonitorTab::Overview;
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.overview.procs_running(), Some(7));
    overlay.handle_key(&KeyCode::Char(' '), NONE);
    assert!(overlay.is_paused());
    let next_frame = thegn_metrics::collect::frame::MonitorFrame {
        parts: thegn_metrics::collect::frame::Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                procs_running: Some(99),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!overlay.ingest_frame(&next_frame));
    assert_eq!(overlay.overview.procs_running(), Some(7));
}

#[test]
fn overview_demand_omits_absent_power_and_is_not_all_collectors() {
    let (mut overlay, model, _history) = open_on(Rect::full(160, 48), StatsSnapshot::default());
    overlay.tab = MonitorTab::Overview;
    overlay.overview.absorb_stats(&model.stats);
    let mut frame = thegn_metrics::collect::frame::MonitorFrame::default();
    frame.health[thegn_metrics::collect::CollectorId::PowerSupply.index()] =
        thegn_metrics::collect::health::Health::Absent(
            thegn_metrics::collect::health::Absent::NoHardware,
        );
    overlay.ingest_frame(&frame);
    let demand = overlay.demand(1000);
    assert!(
        !demand
            .collectors
            .contains(thegn_metrics::collect::CollectorId::PowerSupply)
    );
    assert!(demand.collectors.len() < thegn_metrics::collect::CollectorId::ALL.len());
    assert!(
        !overlay
            .placed_panels()
            .iter()
            .any(|panel| panel.id.0 == "overview.power")
    );
}

#[test]
fn overview_is_one_and_cpu_moves_to_two_while_saved_cpu_slug_still_opens() {
    let (mut overlay, model, history) = open();
    ch(&mut overlay, '1');
    assert_eq!(overlay.tab(), MonitorTab::Overview);
    ch(&mut overlay, '2');
    assert_eq!(overlay.tab(), MonitorTab::Cpu);

    let mut prefs = MonitorPrefs::default();
    prefs.apply("last_tab", "cpu");
    let ctx = ctx_at(&history, Rect::full(120, 40));
    let restored = MonitorOverlay::open(prefs.last_tab, prefs, &model, &ctx);
    assert_eq!(restored.tab(), MonitorTab::Cpu);
}
