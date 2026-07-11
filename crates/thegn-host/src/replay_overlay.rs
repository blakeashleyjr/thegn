//! Time-travel replay mode overlay (`Alt+r`).
//!
//! A modal overlay — the sibling of [`crate::search::SearchOverlay`] — that lets
//! the user scrub a pane's recorded byte stream like a video: play/pause, step,
//! seek, change speed, reverse, skip idle gaps, and search across time for any
//! string that ever appeared on screen (including inside full-screen apps). It
//! paints from a **scratch** [`AlacrittyEmulator`] rebuilt from the pane's
//! [`Recording`] — never the live pane, which keeps advancing underneath.
//!
//! The overlay owns no timer. While playing, a clock thread in `run.rs` (alive
//! only while playing) pulses the `TerminalWaker`; the loop then calls
//! [`ReplayOverlay::advance_clock`], which advances the cursor by *real* elapsed
//! time. Paused or closed ⇒ no thread, no wakeups.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use termwiz::terminal::TerminalWaker;

use crate::chrome::S;
use crate::compositor::{Rect, compose_pane};
use crate::emulator::AlacrittyEmulator;
use crate::replay::{Recording, parse_duration};
use crate::seg::{self, Line, Tok, seg};

/// Seek granularity for the `←`/`→` keys.
const SEEK_STEP_MS: u64 = 5_000;
/// Playback speeds cycled by `[` / `]`.
const SPEEDS: &[f32] = &[0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

/// What [`ReplayOverlay::handle_key`] signals to the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// Still scrubbing — mark dirty and continue.
    Pending,
    /// Exit replay (Esc/`q`) — snap the pane back to its live tail.
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayState {
    Paused,
    Playing,
}

/// The live replay overlay. Lives in `run.rs` as `Option<ReplayOverlay>`.
pub struct ReplayOverlay {
    pane: u32,
    /// The emulator the overlay paints from — rebuilt/advanced from the recording,
    /// never the live pane.
    scratch: AlacrittyEmulator,
    /// Current position in the recording (ms since its epoch).
    cursor_ms: u64,
    /// The scratch emulator has been fed events with `at_ms <= fed_upto`; forward
    /// playback feeds incrementally from here, a backward jump forces a rebuild.
    fed_upto: u64,
    state: PlayState,
    reverse: bool,
    speed: f32,
    /// Wall-clock anchor for the playback clock; `None` when paused.
    last_tick: Option<Instant>,
    idle_threshold_ms: u64,
    /// Search sub-mode: typing a query at the `/` prompt.
    searching: bool,
    query: String,
    last_query: String,
    /// Transient status note (e.g. "skipped 4m idle", "no match").
    note: String,
}

impl ReplayOverlay {
    /// Open replay for `pane`, positioned at the recording's live tail (paused).
    pub fn new(pane: u32, rec: &Recording, idle_threshold_ms: u64) -> Self {
        let cursor_ms = rec.end_ms();
        Self {
            pane,
            scratch: rec.reconstruct(cursor_ms),
            cursor_ms,
            fed_upto: cursor_ms,
            state: PlayState::Paused,
            reverse: false,
            speed: 1.0,
            last_tick: None,
            idle_threshold_ms,
            searching: false,
            query: String::new(),
            last_query: String::new(),
            note: String::new(),
        }
    }

    pub fn pane_id(&self) -> u32 {
        self.pane
    }

    /// Whether the playback clock thread should be running (drives the ticker in
    /// `run.rs`).
    pub fn is_playing(&self) -> bool {
        self.state == PlayState::Playing
    }

    /// Reposition to an absolute time, rebuilding or extending the scratch grid.
    fn seek(&mut self, rec: &Recording, to_ms: u64) {
        let to_ms = to_ms.clamp(rec.start_ms(), rec.end_ms());
        if to_ms >= self.fed_upto {
            // Forward: feed only the new slice into the existing grid.
            rec.feed_into(&mut self.scratch, self.fed_upto, to_ms);
        } else {
            // Backward: a clean rebuild from the retained front.
            self.scratch = rec.reconstruct(to_ms);
        }
        self.cursor_ms = to_ms;
        self.fed_upto = to_ms;
    }

    fn toggle_play(&mut self) {
        match self.state {
            PlayState::Paused => {
                self.state = PlayState::Playing;
                self.last_tick = None; // reset the wall-clock anchor
                self.note.clear();
            }
            PlayState::Playing => {
                self.state = PlayState::Paused;
                self.last_tick = None;
            }
        }
    }

    fn cycle_speed(&mut self, faster: bool) {
        let idx = SPEEDS
            .iter()
            .position(|s| (*s - self.speed).abs() < f32::EPSILON)
            .unwrap_or(2);
        let next = if faster {
            (idx + 1).min(SPEEDS.len() - 1)
        } else {
            idx.saturating_sub(1)
        };
        self.speed = SPEEDS[next];
    }

    /// Advance the playback cursor by real elapsed wall-clock time (scaled by
    /// speed), collapsing idle gaps. Returns `true` if the grid changed (caller
    /// marks dirty). Auto-pauses at the recording's live tail (or start, when
    /// reversing).
    pub fn advance_clock(&mut self, rec: &Recording) -> bool {
        if self.state != PlayState::Playing {
            return false;
        }
        let now = Instant::now();
        let dt_ms = match self.last_tick.replace(now) {
            Some(prev) => {
                (now.saturating_duration_since(prev).as_millis() as f64 * self.speed as f64) as u64
            }
            None => 0, // first tick after play: just anchor the clock
        };

        if self.reverse {
            let target = self.cursor_ms.saturating_sub(dt_ms.max(1));
            if target <= rec.start_ms() {
                self.seek(rec, rec.start_ms());
                self.state = PlayState::Paused;
                self.last_tick = None;
                return true;
            }
            self.seek(rec, target);
            return true;
        }

        // Forward: skip a large idle gap in one hop.
        if let Some(skip_to) = rec.skip_target(self.cursor_ms, self.idle_threshold_ms) {
            let gap = skip_to.saturating_sub(self.cursor_ms);
            self.note = format!("⏩ skipped {}", fmt_dur(gap));
            self.seek(rec, skip_to);
            return true;
        }

        let target = self.cursor_ms.saturating_add(dt_ms);
        if target >= rec.end_ms() {
            self.seek(rec, rec.end_ms());
            self.state = PlayState::Paused; // reached live tail
            self.last_tick = None;
            return true;
        }
        if dt_ms == 0 {
            return false;
        }
        self.seek(rec, target);
        true
    }

    fn run_search(&mut self, rec: &Recording, reverse: bool) {
        let q = if self.query.trim().is_empty() {
            self.last_query.clone()
        } else {
            self.query.trim().to_string()
        };
        if q.is_empty() {
            return;
        }
        self.last_query = q.clone();
        // A bare time expression jumps a fixed delta in the search direction.
        if let Some(d) = parse_duration(&q) {
            let delta = d.as_millis() as u64;
            let to = if reverse {
                self.cursor_ms.saturating_sub(delta)
            } else {
                self.cursor_ms.saturating_add(delta)
            };
            self.seek(rec, to);
            self.note = format!(
                "jumped {}{}",
                if reverse { "-" } else { "+" },
                fmt_dur(delta)
            );
            return;
        }
        match rec.search_next(&q, self.cursor_ms, reverse) {
            Some(at) => {
                self.seek(rec, at);
                self.note = format!("match: {q}");
            }
            None => self.note = format!("no match: {q}"),
        }
    }

    /// Feed a key. `rec` is the pane's recording (bounds + reconstruction).
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers, rec: &Recording) -> ReplayOutcome {
        // ── Search input sub-mode captures typing ──────────────────────────────
        if self.searching {
            if crate::input::is_escape_key(key) {
                self.searching = false;
                self.query.clear();
                return ReplayOutcome::Pending;
            }
            match key {
                KeyCode::Enter => {
                    self.searching = false;
                    self.run_search(rec, false);
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c)
                    if (mods.is_empty() || mods == Modifiers::SHIFT) && !c.is_control() =>
                {
                    self.query.push(*c);
                }
                _ => {}
            }
            return ReplayOutcome::Pending;
        }

        // ── Exit ────────────────────────────────────────────────────────────────
        if crate::input::is_escape_key(key)
            || matches!(key, KeyCode::Char('q'))
            || (mods.contains(Modifiers::CTRL)
                && matches!(key, KeyCode::Char('g') | KeyCode::Char('c')))
        {
            return ReplayOutcome::Dismiss;
        }

        self.note.clear();
        match key {
            KeyCode::Char(' ') => self.toggle_play(),
            KeyCode::LeftArrow => self.seek(rec, self.cursor_ms.saturating_sub(SEEK_STEP_MS)),
            KeyCode::RightArrow => self.seek(rec, self.cursor_ms.saturating_add(SEEK_STEP_MS)),
            // Single-step by recorded event.
            KeyCode::Char('k') | KeyCode::UpArrow => {
                if let Some(t) = rec.prev_event_ms(self.cursor_ms) {
                    self.seek(rec, t);
                }
            }
            KeyCode::Char('j') | KeyCode::DownArrow => {
                if let Some(t) = rec.next_event_ms(self.cursor_ms) {
                    self.seek(rec, t);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.seek(rec, rec.start_ms()),
            KeyCode::Char('G') | KeyCode::End => self.seek(rec, rec.end_ms()),
            KeyCode::Char('[') => self.cycle_speed(false),
            KeyCode::Char(']') => self.cycle_speed(true),
            KeyCode::Char('r') => {
                self.reverse = !self.reverse;
                self.last_tick = None;
            }
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }
            KeyCode::Char('n') => self.run_search(rec, false),
            KeyCode::Char('N') => self.run_search(rec, true),
            _ => {}
        }
        ReplayOutcome::Pending
    }

    /// Draw the scratch grid over `rect`, with a scrub/status bar on the bottom
    /// row (and a search prompt row when typing a query).
    pub fn render(&self, surface: &mut Surface, rect: Rect, rec: &Recording) {
        if rect.rows == 0 || rect.cols == 0 {
            return;
        }
        let bar_rows = if self.searching { 2 } else { 1 };
        let content = Rect {
            x: rect.x,
            y: rect.y,
            cols: rect.cols,
            rows: rect.rows.saturating_sub(bar_rows),
        };
        compose_pane(surface, &self.scratch, content);

        let panel = Tok::Slot(S::Panel);
        let mut by = rect.y + rect.rows.saturating_sub(bar_rows);

        // ── Scrub / status bar ──────────────────────────────────────────────────
        let start = rec.start_ms();
        let end = rec.end_ms();
        let span = end.saturating_sub(start).max(1);
        let frac = (self.cursor_ms.saturating_sub(start)) as f64 / span as f64;
        let bar_w = rect.cols.saturating_sub(2).clamp(4, 40);
        let filled = ((frac * bar_w as f64).round() as usize).min(bar_w);
        // Keyframe positions become faint tick marks so the timeline shows where
        // activity clustered.
        let mut ticks = vec![false; bar_w];
        for kf in rec.keyframe_times() {
            let col = (((kf.saturating_sub(start)) as f64 / span as f64) * bar_w as f64) as usize;
            if let Some(slot) = ticks.get_mut(col.min(bar_w.saturating_sub(1))) {
                *slot = true;
            }
        }
        let track: String = (0..bar_w)
            .map(|i| match (i < filled, ticks[i]) {
                (true, _) => '━',
                (false, true) => '┿',
                (false, false) => '─',
            })
            .collect();

        let icon = match self.state {
            PlayState::Playing if self.reverse => "◀ ",
            PlayState::Playing => "▶ ",
            PlayState::Paused => "⏸ ",
        };
        let clock = format!(" {} / {}  ", fmt_ms(self.cursor_ms), fmt_ms(end));
        let speed = format!("{}x", trim_speed(self.speed));

        let mut left = vec![
            seg(Tok::Slot(S::Accent), icon).bold(),
            seg(Tok::Slot(S::Accent), track),
            seg(Tok::Slot(S::Text), clock),
            seg(Tok::Slot(S::Ghost2), speed),
        ];
        if !self.note.is_empty() {
            left.push(seg(Tok::Slot(S::Ghost3), format!("   {}", self.note)));
        }
        let right = vec![seg(
            Tok::Slot(S::Ghost),
            "space ⏯  ←→ seek  j/k step  [ ] speed  r rev  / find  q quit",
        )];
        seg::draw_line(
            surface,
            rect.x,
            by,
            rect.cols,
            &Line::split(left, right),
            panel,
        );

        // ── Search prompt row ─────────────────────────────────────────────────
        if self.searching {
            by += 1;
            let prompt = vec![
                seg(Tok::Slot(S::Accent), "/ ").bold(),
                seg(Tok::Slot(S::Text), self.query.clone()),
                seg(Tok::Slot(S::Accent), "█"),
            ];
            seg::draw_line(surface, rect.x, by, rect.cols, &Line::segs(prompt), panel);
        }
    }
}

/// Frame cadence of the playback clock (~30 fps). Playback speed scales the
/// per-tick step, not the tick rate, so this stays constant.
pub const REPLAY_FRAME_DT_MS: u64 = 33;

#[derive(Default)]
struct ClockState {
    playing: bool,
    frame_dt_ms: u64,
}

/// The one timer in replay: a thread that pulses the `TerminalWaker` at frame
/// cadence **only while playing**, and parks on a condvar (zero CPU, zero
/// wakeups) when paused or the overlay is closed. It is an event producer, not a
/// poll — the "no polling timeout" invariant holds. Session-lifetime; parking is
/// the "stopped" state.
pub struct PlaybackClock {
    inner: Arc<(Mutex<ClockState>, Condvar)>,
}

impl PlaybackClock {
    pub fn spawn(waker: TerminalWaker) -> Self {
        let inner = Arc::new((
            Mutex::new(ClockState {
                playing: false,
                frame_dt_ms: REPLAY_FRAME_DT_MS,
            }),
            Condvar::new(),
        ));
        let t = inner.clone();
        std::thread::spawn(move || {
            let (lock, cv) = &*t;
            loop {
                let dt = {
                    let mut g = lock.lock().unwrap();
                    // Park until playback starts — no wakeups, no CPU.
                    while !g.playing {
                        g = cv.wait(g).unwrap();
                    }
                    g.frame_dt_ms.max(1)
                };
                std::thread::sleep(std::time::Duration::from_millis(dt));
                let _ = waker.wake();
            }
        });
        Self { inner }
    }

    /// Set the desired playing state / cadence. Notifies the thread when playback
    /// (re)starts so it leaves the park.
    pub fn set(&self, playing: bool, frame_dt_ms: u64) {
        let (lock, cv) = &*self.inner;
        let mut g = lock.lock().unwrap();
        let was = g.playing;
        g.playing = playing;
        g.frame_dt_ms = frame_dt_ms;
        drop(g);
        if playing && !was {
            cv.notify_all();
        }
    }
}

/// `mm:ss` (or `h:mm:ss`) for a millisecond offset.
fn fmt_ms(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A compact human duration for the skip/jump note.
fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s > 0 {
        format!("{s}s")
    } else {
        format!("{ms}ms")
    }
}

fn trim_speed(sp: f32) -> String {
    if (sp.fract()).abs() < f32::EPSILON {
        format!("{}", sp as i32)
    } else {
        format!("{sp}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::grid_text;
    use std::time::Duration;
    use thegn_core::config::ReplayConfig;

    fn recording_with(script: &[(&[u8], u64)]) -> Recording {
        let mut rec = Recording::from_config(&ReplayConfig::default(), 24, 80);
        for (bytes, at) in script {
            // Drive at explicit offsets via the public API by faking elapsed time:
            // push_bytes stamps against Instant::now(), so we can't set arbitrary
            // times here — instead assert via the recording's own ordering.
            rec.push_bytes(
                bytes,
                std::time::Instant::now() + Duration::from_millis(*at),
            );
        }
        rec
    }

    #[test]
    fn opens_paused_at_live_tail() {
        let rec = recording_with(&[(b"hello\r\n", 10), (b"world\r\n", 20)]);
        let ov = ReplayOverlay::new(7, &rec, 1000);
        assert_eq!(ov.pane_id(), 7);
        assert!(!ov.is_playing());
        assert_eq!(ov.cursor_ms, rec.end_ms());
        // The scratch grid shows the tail content.
        assert!(grid_text(&ov.scratch).contains("world"));
    }

    #[test]
    fn space_toggles_play_pause() {
        let rec = recording_with(&[(b"x", 10)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        assert!(!ov.is_playing());
        ov.handle_key(&KeyCode::Char(' '), Modifiers::NONE, &rec);
        assert!(ov.is_playing());
        ov.handle_key(&KeyCode::Char(' '), Modifiers::NONE, &rec);
        assert!(!ov.is_playing());
    }

    #[test]
    fn esc_and_q_dismiss() {
        let rec = recording_with(&[(b"x", 10)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        assert_eq!(
            ov.handle_key(&KeyCode::Char('q'), Modifiers::NONE, &rec),
            ReplayOutcome::Dismiss
        );
        assert_eq!(
            ov.handle_key(&KeyCode::Escape, Modifiers::NONE, &rec),
            ReplayOutcome::Dismiss
        );
    }

    #[test]
    fn g_and_shift_g_jump_to_bounds() {
        let rec = recording_with(&[(b"a\r\n", 10), (b"b\r\n", 20), (b"c\r\n", 30)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        ov.handle_key(&KeyCode::Char('g'), Modifiers::NONE, &rec);
        assert_eq!(ov.cursor_ms, rec.start_ms());
        ov.handle_key(&KeyCode::Char('G'), Modifiers::NONE, &rec);
        assert_eq!(ov.cursor_ms, rec.end_ms());
    }

    #[test]
    fn speed_cycles_within_bounds() {
        let rec = recording_with(&[(b"x", 10)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        assert!((ov.speed - 1.0).abs() < f32::EPSILON);
        ov.handle_key(&KeyCode::Char(']'), Modifiers::NONE, &rec);
        assert!((ov.speed - 2.0).abs() < f32::EPSILON);
        // Down past the floor stays at the slowest.
        for _ in 0..10 {
            ov.handle_key(&KeyCode::Char('['), Modifiers::NONE, &rec);
        }
        assert!((ov.speed - SPEEDS[0]).abs() < f32::EPSILON);
    }

    #[test]
    fn search_submode_types_then_jumps() {
        let rec = recording_with(&[(b"alpha\r\n", 10), (b"beta\r\n", 20)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        // Start at the tail; reverse-search for alpha via / then n? Use forward
        // from start: jump to start first.
        ov.handle_key(&KeyCode::Char('g'), Modifiers::NONE, &rec);
        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE, &rec);
        for c in "beta".chars() {
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE, &rec);
        }
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &rec);
        assert!(!ov.searching);
        // Landed on a frame that shows "beta".
        assert!(grid_text(&ov.scratch).contains("beta"));
    }

    #[test]
    fn render_does_not_panic_on_tiny_rect() {
        let rec = recording_with(&[(b"hello", 10)]);
        let ov = ReplayOverlay::new(1, &rec, 1000);
        let mut surface = Surface::new(10, 3);
        ov.render(
            &mut surface,
            Rect {
                x: 0,
                y: 0,
                cols: 10,
                rows: 3,
            },
            &rec,
        );
    }

    #[test]
    fn fmt_ms_formats() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(65_000), "1:05");
        assert_eq!(fmt_ms(3_661_000), "1:01:01");
    }
}
