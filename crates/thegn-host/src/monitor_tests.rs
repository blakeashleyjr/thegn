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
    // Alt/Super chords belong to the compositor — swallowed, not acted on.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('p'), Modifiers::ALT),
        MonitorOutcome::Pending
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('x'), Modifiers::SUPER),
        MonitorOutcome::Pending
    );
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
    let visible = MonitorTab::visible(&bare);
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
    assert!(MonitorTab::visible(&with_gpu).contains(&MonitorTab::Gpu));
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
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::All);
    ch(&mut ov, ']');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::All);
    for _ in 0..10 {
        ch(&mut ov, '[');
    }
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::Short);
    ch(&mut ov, '[');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::Short);
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
    // the wake rate either.
    let (mut ov, _m, _h) = open();
    assert!(ov.wants_live());
    for _ in 0..4 {
        ch(&mut ov, ']');
    }
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::All);
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
    assert!(text.contains("10m"), "window not shown: {text}");
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
    for _ in 0..4 {
        ch(&mut ov, ']');
    }
    ch(&mut ov, '[');
    assert_eq!(ov.prefs.tab(MonitorTab::Cpu).window, Window::Hour);
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
