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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use termwiz::terminal::TerminalWaker;
use thegn_core::config::ReplayConfig;

use crate::chrome::S;
use crate::compositor::{Rect, compose_pane};
use crate::emulator::AlacrittyEmulator;
use crate::replay::{Recording, SearchResult, SearchSnapshot, parse_duration};
use crate::seg::{self, Line, Tok, seg};

/// Seek granularity for the `←`/`→` keys.
const SEEK_STEP_MS: u64 = 5_000;
const PARTIAL_HISTORY_BADGE: &str = " partial history ";
/// Cap query normalization and substring work independently of recording size.
const SEARCH_MAX_QUERY_BYTES: usize = 1024;
type ReplayConfigVersion = (bool, u64, u64, u64, u64, u64, bool);

fn replay_config_version(cfg: &ReplayConfig) -> ReplayConfigVersion {
    (
        cfg.enabled,
        cfg.max_bytes_per_pane,
        cfg.max_duration_secs,
        cfg.keyframe_interval_ms,
        cfg.keyframe_interval_bytes,
        cfg.idle_threshold_ms,
        cfg.persist,
    )
}
/// Playback speeds cycled by `[` / `]`.
const SPEEDS: &[f32] = &[0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

/// What [`ReplayOverlay::handle_key`] signals to the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// Still scrubbing — mark dirty and continue.
    Pending,
    /// Export the retained recording as a `.cast` file. The run loop does the
    /// file I/O (it holds the config + recordings dir) and calls
    /// [`ReplayOverlay::set_note`] with the result; the overlay stays open.
    Export,
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
    /// Absolute sequence of the next event to feed into `scratch`. Timestamp
    /// alone cannot distinguish events sharing one millisecond.
    fed_seq: u64,
    /// Retained front sequence used to build the current scratch emulator.
    retained_floor_seq: u64,
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
    search_worker: Option<SearchWorker>,
    search_generation: u64,
    search_recording_version: Option<(u64, u64)>,
    search_config_version: Option<ReplayConfigVersion>,
}

impl ReplayOverlay {
    /// Open replay for `pane`, positioned at the recording's live tail (paused).
    pub fn new(pane: u32, rec: &Recording, idle_threshold_ms: u64) -> Self {
        let cursor_ms = rec.end_ms();
        let (scratch, fed_seq) = rec.reconstruct_with_cursor(cursor_ms);
        let retained_floor_seq = rec.first_sequence();
        Self {
            pane,
            scratch,
            cursor_ms,
            fed_seq,
            retained_floor_seq,
            state: PlayState::Paused,
            reverse: false,
            speed: 1.0,
            last_tick: None,
            idle_threshold_ms,
            searching: false,
            query: String::new(),
            last_query: String::new(),
            note: String::new(),
            search_worker: None,
            search_generation: 0,
            search_recording_version: None,
            search_config_version: None,
        }
    }

    /// Install the loop waker once the overlay enters the event loop. The worker
    /// is lazy with respect to searches, but its thread is owned by this overlay.
    pub fn set_search_waker(&mut self, waker: TerminalWaker) {
        if self.search_worker.is_none() {
            self.search_worker = SearchWorker::spawn(waker);
        }
    }

    /// Remember the replay config generation present when the overlay opened.
    pub fn set_search_config(&mut self, cfg: &ReplayConfig) {
        self.search_config_version = Some(replay_config_version(cfg));
    }

    pub fn pane_id(&self) -> u32 {
        self.pane
    }

    /// Set the transient status note (the run loop uses this to report the
    /// `.cast` export result over the scrub bar).
    pub fn set_note(&mut self, note: String) {
        self.note = note;
    }

    /// Whether the playback clock thread should be running (drives the ticker in
    /// `run.rs`).
    pub fn is_playing(&self) -> bool {
        self.state == PlayState::Playing
    }

    /// Reposition to an absolute time, rebuilding or extending the scratch grid.
    fn seek(&mut self, rec: &Recording, to_ms: u64) {
        let mut to_ms = to_ms.clamp(rec.start_ms(), rec.end_ms());
        if self.retained_floor_seq != rec.first_sequence() {
            // Retention overtook the incremental cursor. Rebase to the retained
            // front so no evicted state is assumed to still be available.
            to_ms = rec.start_ms();
            (self.scratch, self.fed_seq) = rec.reconstruct_with_cursor(to_ms);
            self.retained_floor_seq = rec.first_sequence();
        } else if to_ms >= self.cursor_ms {
            // Forward: consume only unseen sequence positions, including any
            // newly appended events in the current millisecond.
            self.fed_seq = rec.feed_from_sequence(&mut self.scratch, self.fed_seq, to_ms);
        } else {
            // Backward: a clean rebuild from the retained front.
            (self.scratch, self.fed_seq) = rec.reconstruct_with_cursor(to_ms);
            self.retained_floor_seq = rec.first_sequence();
        }
        self.cursor_ms = to_ms;
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
        self.cancel_search();
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
        let Some(worker) = self.search_worker.as_mut() else {
            self.note = "search unavailable".into();
            return;
        };
        worker.submit(
            self.search_generation,
            rec.version(),
            q,
            self.cursor_ms,
            reverse,
            rec.search_snapshot(),
        );
        self.search_recording_version = Some(rec.version());
        self.note = "searching…".into();
    }

    /// Drain a completed search on the loop. Results are applied only for the
    /// current recording identity and query generation.
    pub fn drain_search_result(&mut self, rec: &Recording) -> bool {
        let Some(worker) = self.search_worker.as_mut() else {
            return false;
        };
        let Ok(result) = worker.try_result() else {
            return false;
        };
        let (generation, recording_id, query, result) = result;
        if generation != self.search_generation
            || recording_id.0 != rec.version().0
            || query != self.last_query
        {
            return false;
        }
        self.search_recording_version = None;
        match result {
            SearchResult::Match(at) => {
                if at < rec.start_ms() {
                    self.note = format!("match evicted: {query}");
                } else {
                    self.seek(rec, at);
                    self.note = format!("match: {query}");
                }
            }
            SearchResult::PartialMatch(at) => {
                if at < rec.start_ms() {
                    self.note = format!("match evicted: {query} (partial history)");
                } else {
                    self.seek(rec, at);
                    self.note = format!("match: {query} (partial history)");
                }
            }
            SearchResult::NoMatch => self.note = format!("no match: {query}"),
            SearchResult::PartialNoMatch => {
                self.note = format!("no match: {query} (partial history)")
            }
            SearchResult::Incomplete => self.note = format!("search incomplete: {query}"),
            SearchResult::Cancelled => return false,
        }
        true
    }

    /// Config reload may replace a pane's recording while replay stays open.
    /// Appends and ring eviction keep the same identity and do not cancel work.
    pub fn invalidate_changed_recording(&mut self, rec: &Recording) -> bool {
        if self
            .search_recording_version
            .is_some_and(|version| version.0 != rec.version().0)
        {
            self.cancel_search();
            self.note = "search cancelled: recording changed".into();
            return true;
        }
        false
    }

    /// Cancel active work if live replay settings changed during a search.
    pub fn invalidate_changed_config(&mut self, cfg: &ReplayConfig) -> bool {
        let version = replay_config_version(cfg);
        let changed = self
            .search_config_version
            .is_some_and(|previous| previous != version);
        self.search_config_version = Some(version);
        if changed && self.search_recording_version.is_some() {
            self.cancel_search();
            self.note = "search cancelled: replay config changed".into();
            return true;
        }
        false
    }

    /// Feed a key. `rec` is the pane's recording (bounds + reconstruction).
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers, rec: &Recording) -> ReplayOutcome {
        // ── Search input sub-mode captures typing ──────────────────────────────
        if self.searching {
            if crate::input::is_escape_key(key) {
                self.cancel_search();
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
                    self.cancel_search();
                    self.query.pop();
                }
                KeyCode::Char(c)
                    if (mods.is_empty() || mods == Modifiers::SHIFT) && !c.is_control() =>
                {
                    self.cancel_search();
                    if self.query.len().saturating_add(c.len_utf8()) <= SEARCH_MAX_QUERY_BYTES {
                        self.query.push(*c);
                    } else {
                        self.note = format!("search query limit: {SEARCH_MAX_QUERY_BYTES} bytes");
                    }
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
            self.cancel_search();
            return ReplayOutcome::Dismiss;
        }

        self.note.clear();
        if !matches!(key, KeyCode::Char('n') | KeyCode::Char('N')) {
            self.cancel_search();
        }
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
                self.cancel_search();
                self.searching = true;
                self.query.clear();
            }
            KeyCode::Char('n') => self.run_search(rec, false),
            KeyCode::Char('N') => self.run_search(rec, true),
            // Export the retained window as a `.cast` (the run loop finishes it).
            KeyCode::Char('e') => return ReplayOutcome::Export,
            _ => {}
        }
        ReplayOutcome::Pending
    }

    fn cancel_search(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        if let Some(worker) = self.search_worker.as_mut() {
            worker.cancel();
        }
        self.search_recording_version = None;
    }

    /// Draw the scratch grid over `rect`, with a scrub/status bar on the bottom
    /// row (and a search prompt row when typing a query).
    pub fn render(&self, surface: &mut Surface, rect: Rect, rec: &Recording) {
        if rect.rows == 0 || rect.cols == 0 {
            return;
        }
        // One of the two overlays that bypasses `layer::open_layer` (it blits a
        // raw grid, with no box to draw), so it registers its own cover — the
        // focused pane's caret must not show through the replay.
        crate::caret::cover(rect);

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
        let full_bar_w = rect.cols.saturating_sub(2).clamp(4, 40);
        let badge_w = if rec.history_truncated() {
            PARTIAL_HISTORY_BADGE.len()
        } else {
            0
        };
        // The persistent badge takes its width from the scrub track, so the
        // truncated layout retains the same left-side footprint as an intact
        // recording. The small-width floor keeps the track useful in narrow
        // overlays.
        let bar_w = full_bar_w.saturating_sub(badge_w).max(4);
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

        let mut left = vec![seg(Tok::Slot(S::Accent), icon).bold()];
        if rec.history_truncated() {
            left.push(seg(Tok::Slot(S::Ghost3), PARTIAL_HISTORY_BADGE));
        }
        left.extend([
            seg(Tok::Slot(S::Accent), track),
            seg(Tok::Slot(S::Text), clock),
            seg(Tok::Slot(S::Ghost2), speed),
        ]);
        if !self.note.is_empty() {
            left.push(seg(Tok::Slot(S::Ghost3), format!("   {}", self.note)));
        }
        let right = vec![seg(
            Tok::Slot(S::Ghost),
            "space ⏯  ←→ seek  j/k step  [ ] speed  r rev  / find  e export  q quit",
        )];
        let min_left = seg::seg_width(&left).min(rect.cols);
        seg::draw_line(
            surface,
            rect.x,
            by,
            rect.cols,
            &Line::SplitMinLeft {
                l: left,
                r: right,
                min_l: min_left,
            },
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

struct SearchJob {
    generation: u64,
    recording_version: (u64, u64),
    query: String,
    from_ms: u64,
    reverse: bool,
    snapshot: SearchSnapshot,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct SearchWorkerState {
    pending: Option<SearchJob>,
    stopped: bool,
}

/// One parked, overlay-owned worker. It runs at most one scan and retains at
/// most one replaceable pending snapshot. Including the loop's temporary
/// capture, peak snapshot ownership is three bounded copies regardless of
/// submission rate; no thread is created per query.
struct SearchWorker {
    state: Arc<(Mutex<SearchWorkerState>, Condvar)>,
    results: Receiver<(u64, (u64, u64), String, SearchResult)>,
    active_cancel: Option<Arc<AtomicBool>>,
}

impl SearchWorker {
    fn spawn(waker: TerminalWaker) -> Option<Self> {
        let state = Arc::new((Mutex::new(SearchWorkerState::default()), Condvar::new()));
        let thread_state = state.clone();
        let (result_tx, results) = mpsc::sync_channel(1);
        match thegn_core::thread_name::try_spawn_named("thegn-replay-sr", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            loop {
                let job = {
                    let (lock, cv) = &*thread_state;
                    let mut guard = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while guard.pending.is_none() && !guard.stopped {
                        guard = cv
                            .wait(guard)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if guard.stopped {
                        return;
                    }
                    guard.pending.take().expect("pending job checked")
                };
                let result =
                    job.snapshot
                        .search(&job.query, job.from_ms, job.reverse, &job.cancelled);
                if result != SearchResult::Cancelled
                    && !job.cancelled.load(Ordering::Relaxed)
                    && result_tx
                        .send((job.generation, job.recording_version, job.query, result))
                        .is_ok()
                    && let Err(error) = waker.wake()
                {
                    tracing::debug!(%error, "could not wake loop for replay search result");
                }
            }
        }) {
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "could not spawn replay search worker");
                return None;
            }
        }
        Some(Self {
            state,
            results,
            active_cancel: None,
        })
    }

    fn submit(
        &mut self,
        generation: u64,
        recording_version: (u64, u64),
        query: String,
        from_ms: u64,
        reverse: bool,
        snapshot: SearchSnapshot,
    ) {
        self.cancel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active_cancel = Some(cancelled.clone());
        let (lock, cv) = &*self.state;
        let mut guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.pending = Some(SearchJob {
            generation,
            recording_version,
            query,
            from_ms,
            reverse,
            snapshot,
            cancelled,
        });
        drop(guard);
        cv.notify_one();
    }

    fn cancel(&mut self) {
        if let Some(cancelled) = self.active_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        let (lock, _) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending = None;
    }

    fn try_result(&self) -> Result<(u64, (u64, u64), String, SearchResult), TryRecvError> {
        self.results.try_recv()
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.cancel();
        let (lock, cv) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stopped = true;
        cv.notify_one();
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
        thegn_core::thread_name::spawn_named("thegn-replay", move || {
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
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
        let base = Instant::now() + Duration::from_secs(1);
        for (bytes, at) in script {
            rec.push_bytes(bytes, base + Duration::from_millis(*at));
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
    fn same_timestamp_seek_and_resume_use_sequence_cursor() {
        let mut rec = Recording::from_config(&ReplayConfig::default(), 24, 80);
        let base = Instant::now() + Duration::from_secs(1);
        rec.push_bytes(b"one\r\n", base);
        rec.push_bytes(b"two\r\n", base);
        let zeroish = rec.start_ms();
        let mut ov = ReplayOverlay::new(1, &rec, 1000);

        ov.seek(&rec, zeroish);
        let at_start = grid_text(&ov.scratch);
        assert!(at_start.contains("one"));
        assert!(at_start.contains("two"));
        ov.seek(&rec, zeroish);
        assert_eq!(grid_text(&ov.scratch), at_start);

        // A later PTY read can append another event in the cursor's current
        // millisecond. Advancing to that timestamp must consume its new sequence.
        rec.push_bytes(b"resumed\r\n", base);
        ov.seek(&rec, zeroish);
        assert!(grid_text(&ov.scratch).contains("resumed"));
        rec.push_bytes(b"three\r\n", base + Duration::from_millis(10));
        ov.seek(&rec, rec.end_ms());
        assert!(grid_text(&ov.scratch).contains("three"));
        ov.seek(&rec, zeroish);
        let after_backward_seek = grid_text(&ov.scratch);
        assert!(after_backward_seek.contains("one"));
        assert!(after_backward_seek.contains("two"));
        assert!(after_backward_seek.contains("resumed"));
        assert!(!after_backward_seek.contains("three"));
    }

    #[test]
    fn playback_consumes_same_timestamp_append_at_live_tail_once() {
        let mut rec = Recording::from_config(&ReplayConfig::default(), 24, 80);
        let epoch = rec.epoch();
        rec.push_bytes(b"before\r\n", epoch);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        rec.push_bytes(b"appended\r\n", epoch);

        // The first playback clock tick has zero elapsed time. Reaching the
        // current live tail must still consume the new sequence at cursor_ms.
        ov.toggle_play();
        assert!(ov.advance_clock(&rec));
        let after_tick = grid_text(&ov.scratch);
        assert!(after_tick.contains("before"));
        assert!(after_tick.contains("appended"));
        assert!(
            !ov.is_playing(),
            "the live tail pauses after being consumed"
        );

        // A later clock notification cannot feed the same event a second time.
        assert!(!ov.advance_clock(&rec));
        assert_eq!(grid_text(&ov.scratch), after_tick);
    }

    #[test]
    fn seek_rebuilds_when_eviction_overtakes_sequence_cursor() {
        // Each retained event/keyframe is charged a logical overhead (64 bytes
        // each), so the first chunk costs ~133 and a second pushes past 160:
        // the budget holds exactly one chunk, forcing the second push to evict
        // the first. (A budget of 1 now holds nothing, so the recording would
        // already be empty when the overlay opens.)
        let cfg = ReplayConfig {
            max_bytes_per_pane: 160,
            ..ReplayConfig::default()
        };
        let mut rec = Recording::from_config(&cfg, 24, 80);
        let base = Instant::now() + Duration::from_secs(1);
        rec.push_bytes(b"old\r\n", base);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let old_cursor = ov.cursor_ms;

        rec.push_bytes(b"new\r\n", base + Duration::from_millis(10));
        assert!(rec.first_sequence() > ov.retained_floor_seq);
        ov.seek(&rec, old_cursor);

        assert_eq!(ov.cursor_ms, rec.start_ms());
        assert!(grid_text(&ov.scratch).contains("new"));
        assert!(!grid_text(&ov.scratch).contains("old"));
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
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let rec = recording_with(&[(b"alpha\r\n", 10), (b"beta\r\n", 20)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        ov.set_search_waker(waker);
        // Start at the tail; reverse-search for alpha via / then n? Use forward
        // from start: jump to start first.
        ov.handle_key(&KeyCode::Char('g'), Modifiers::NONE, &rec);
        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE, &rec);
        for c in "beta".chars() {
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE, &rec);
        }
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &rec);
        assert!(!ov.searching);
        assert_eq!(ov.note, "searching…", "search work returns before the scan");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while ov.note == "searching…" && Instant::now() < deadline {
            ov.drain_search_result(&rec);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(ov.note, "match: beta");
        // Landed on a frame that shows "beta".
        assert!(grid_text(&ov.scratch).contains("beta"));
    }

    #[test]
    fn rapid_query_replacement_publishes_only_the_latest_result() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let rec = recording_with(&[(b"alpha\r\n", 10), (b"beta\r\n", 20)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        ov.set_search_waker(waker);
        ov.handle_key(&KeyCode::Char('g'), Modifiers::NONE, &rec);

        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE, &rec);
        for c in "alpha".chars() {
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE, &rec);
        }
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &rec);
        let stale_cancel = ov
            .search_worker
            .as_ref()
            .and_then(|worker| worker.active_cancel.clone())
            .expect("submitted search owns a cancel flag");

        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE, &rec);
        assert!(stale_cancel.load(Ordering::Relaxed));
        for c in "beta".chars() {
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE, &rec);
        }
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &rec);

        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while ov.note == "searching…" && Instant::now() < deadline {
            ov.drain_search_result(&rec);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(ov.note, "match: beta");
        assert!(grid_text(&ov.scratch).contains("beta"));
    }

    #[test]
    fn search_survives_append_with_eviction_and_reports_evicted_match() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let mut rec = Recording::from_config(
            &ReplayConfig {
                max_bytes_per_pane: 256,
                keyframe_interval_bytes: u64::MAX,
                keyframe_interval_ms: u64::MAX,
                ..ReplayConfig::default()
            },
            24,
            80,
        );
        let epoch = rec.epoch();
        rec.push_bytes(b"old", epoch);
        rec.push_bytes(b"\r\nnew", epoch + std::time::Duration::from_millis(10));
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        ov.set_search_waker(waker);
        ov.last_query = "old".into();
        ov.run_search(&rec, true);
        let old_version = ov.search_recording_version;

        rec.push_bytes(&[b'x'; 1024], epoch + std::time::Duration::from_millis(20));
        assert_ne!(old_version, Some(rec.version()));
        assert!(!ov.invalidate_changed_recording(&rec));
        assert_eq!(ov.note, "searching…");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while ov.note == "searching…" && Instant::now() < deadline {
            ov.drain_search_result(&rec);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(ov.note, "match evicted: old");
    }

    #[test]
    fn replay_config_change_invalidates_an_in_flight_search() {
        let rec = recording_with(&[(b"old", 10)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let original = ReplayConfig::default();
        ov.set_search_config(&original);
        ov.search_recording_version = Some(rec.version());
        let changed = ReplayConfig {
            max_bytes_per_pane: original.max_bytes_per_pane + 1,
            ..original
        };

        assert!(ov.invalidate_changed_config(&changed));
        assert!(ov.search_recording_version.is_none());
        assert_eq!(ov.note, "search cancelled: replay config changed");
    }

    #[test]
    fn dropping_overlay_cancels_and_retires_its_worker() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let rec = recording_with(&[(b"alpha", 10)]);
        let mut ov = ReplayOverlay::new(1, &rec, 1000);
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        ov.set_search_waker(waker);
        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE, &rec);
        ov.handle_key(&KeyCode::Char('a'), Modifiers::NONE, &rec);
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &rec);
        let worker = ov.search_worker.as_ref().expect("worker installed");
        let cancelled = worker
            .active_cancel
            .as_ref()
            .expect("search submitted")
            .clone();
        let state = worker.state.clone();
        drop(ov);

        assert!(cancelled.load(Ordering::Relaxed));
        let guard = state.0.lock().unwrap();
        assert!(guard.stopped);
        assert!(guard.pending.is_none());
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
    fn render_shows_partial_history_badge_only_after_boundary_loss() {
        let mut truncated = Recording::from_config(
            &ReplayConfig {
                max_bytes_per_pane: 140,
                max_duration_secs: 0,
                ..ReplayConfig::default()
            },
            24,
            80,
        );
        let epoch = truncated.epoch();
        truncated.push_bytes(b"\x1b[2;3H  ", epoch);
        truncated.push_bytes(b"X", epoch + std::time::Duration::from_millis(1));
        let truncated_overlay = ReplayOverlay::new(1, &truncated, 1000);
        let mut truncated_surface = Surface::new(80, 5);
        truncated_overlay.render(
            &mut truncated_surface,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 5,
            },
            &truncated,
        );
        let truncated_text: String = truncated_surface
            .screen_cells()
            .iter()
            .map(|row| row.iter().map(|cell| cell.str()).collect::<String>())
            .collect::<Vec<_>>()
            .concat();
        assert!(truncated_text.contains("partial history"));
        let track_cells = truncated_text
            .chars()
            .filter(|ch| matches!(ch, '━' | '┿' | '─'))
            .count();
        assert!(track_cells > 12);

        let fresh = recording_with(&[(b"hello", 10)]);
        let fresh_overlay = ReplayOverlay::new(1, &fresh, 1000);
        let mut fresh_surface = Surface::new(80, 5);
        fresh_overlay.render(
            &mut fresh_surface,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 5,
            },
            &fresh,
        );
        let fresh_text: String = fresh_surface
            .screen_cells()
            .iter()
            .map(|row| row.iter().map(|cell| cell.str()).collect::<String>())
            .collect::<Vec<_>>()
            .concat();
        assert!(!fresh_text.contains("partial history"));
        let fresh_track_cells = fresh_text
            .chars()
            .filter(|ch| matches!(ch, '━' | '┿' | '─'))
            .count();
        assert_eq!(
            track_cells + PARTIAL_HISTORY_BADGE.len(),
            fresh_track_cells,
            "the badge should consume only its own width from the scrub track"
        );
    }

    #[test]
    fn fmt_ms_formats() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(65_000), "1:05");
        assert_eq!(fmt_ms(3_661_000), "1:01:01");
    }
}
