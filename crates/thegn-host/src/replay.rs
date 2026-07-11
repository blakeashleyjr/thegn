//! Per-pane time-travel recording ("replay mode", `Alt+r`).
//!
//! Every byte a pane emits already funnels through the single `PtyPane::feed`
//! (`pane.rs`) call — the natural recording tap. A [`Recording`] is a third sink
//! there, alongside the emulator and the ANSI-stripped history ring: it appends
//! each output chunk as a timestamped [`Event`] into a bounded ring, plus a
//! periodic [`Keyframe`] marker into the byte log so the scrubber has a timeline.
//!
//! Seeking to a time T reconstructs the pane's grid by spinning up a **fresh**
//! [`AlacrittyEmulator`] and re-feeding the retained byte slice up to T — the
//! same bytes through the same parser, so the grid is exact within the retained
//! window (no [`PaneEmulator`] trait changes, no grid serialization). The ring is
//! bounded by both a byte and a duration budget; eviction drops oldest events and
//! any keyframe whose byte range no longer exists.
//!
//! When `[replay] enabled = false` no [`Recording`] is allocated and `feed` does
//! a single null check — recording is free when off. This is distinct from the
//! whole-session asciinema [`crate::recorder::Recorder`] (`Ctrl+Alt+r`), which
//! writes a `.cast` file for external playback.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use thegn_core::config::ReplayConfig;

use crate::emulator::{AlacrittyEmulator, PaneEmulator};

/// Scrollback the reconstruction emulator is built with. Replay re-feeds from the
/// retained front, so this only affects how far a paused scrub can scroll up.
const REPLAY_SCROLLBACK: usize = 10_000;

/// The two ways the recorder bounds a pane's ring, plus the keyframe cadence.
#[derive(Debug, Clone, Copy)]
struct Budget {
    max_bytes: u64,
    max_duration_ms: u64,
    keyframe_interval_ms: u64,
    keyframe_interval_bytes: u64,
}

/// One recorded pane event, stamped with ms since the recording epoch.
#[derive(Debug, Clone)]
struct Event {
    at_ms: u64,
    kind: EventKind,
}

#[derive(Debug, Clone)]
enum EventKind {
    /// Raw PTY output bytes fed to the emulator.
    Bytes(Box<[u8]>),
    /// A resize, replayed so the scratch emulator matches the pane's geometry.
    Resize { rows: u16, cols: u16 },
}

/// A byte-log marker (not a grid dump): "at `at_ms`, the ring was at `event_seq`".
/// `event_seq` lets eviction drop keyframes whose byte range is gone; `at_ms`
/// drives the scrubber-timeline tick marks. Reconstruction itself re-feeds from
/// the retained front, so a keyframe is purely an index, never a state dump.
#[derive(Debug, Clone, Copy)]
struct Keyframe {
    at_ms: u64,
    event_seq: u64,
}

/// One recorded pane session — a bounded ring of timestamped byte/resize events
/// with periodic keyframe markers. Lives beside the `PtyPane` as
/// `record: Option<Recording>`.
pub struct Recording {
    epoch: Instant,
    events: VecDeque<Event>,
    keyframes: Vec<Keyframe>,
    budget: Budget,
    /// Number of events dropped off the front (so `seq - evicted` is the deque
    /// index). Also the next sequence number to assign.
    evicted: u64,
    next_seq: u64,
    bytes_used: u64,
    bytes_since_keyframe: u64,
    last_keyframe_ms: u64,
    /// Geometry at the retained front — the dims a reconstruction starts from,
    /// updated when a `Resize` event is evicted so it stays the true baseline.
    base_rows: u16,
    base_cols: u16,
}

impl Recording {
    /// A recording sized from `[replay]` config, starting at the given geometry.
    pub fn from_config(cfg: &ReplayConfig, rows: u16, cols: u16) -> Self {
        Self {
            epoch: Instant::now(),
            events: VecDeque::new(),
            keyframes: Vec::new(),
            budget: Budget {
                max_bytes: cfg.max_bytes_per_pane,
                max_duration_ms: cfg.max_duration_secs.saturating_mul(1000),
                keyframe_interval_ms: cfg.keyframe_interval_ms.max(1),
                keyframe_interval_bytes: cfg.keyframe_interval_bytes.max(1),
            },
            evicted: 0,
            next_seq: 0,
            bytes_used: 0,
            bytes_since_keyframe: 0,
            last_keyframe_ms: 0,
            base_rows: rows.max(1),
            base_cols: cols.max(1),
        }
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_millis() as u64
    }

    /// Append a chunk of PTY output. `now` is read once by the caller (a vDSO
    /// read on the loop — no syscall, no wakeup).
    pub fn push_bytes(&mut self, bytes: &[u8], now: Instant) {
        if bytes.is_empty() {
            return;
        }
        let at_ms = self.elapsed_ms(now);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes_used += bytes.len() as u64;
        self.bytes_since_keyframe += bytes.len() as u64;
        self.events.push_back(Event {
            at_ms,
            kind: EventKind::Bytes(bytes.into()),
        });
        self.maybe_keyframe(at_ms, seq);
        self.evict(at_ms);
    }

    /// Record a resize so replay re-`resize()`s the scratch emulator at the right
    /// moment.
    pub fn record_resize(&mut self, rows: u16, cols: u16, now: Instant) {
        let at_ms = self.elapsed_ms(now);
        self.next_seq += 1;
        self.events.push_back(Event {
            at_ms,
            kind: EventKind::Resize { rows, cols },
        });
        self.evict(at_ms);
    }

    fn maybe_keyframe(&mut self, at_ms: u64, seq: u64) {
        let due_time = self.keyframes.is_empty()
            || at_ms.saturating_sub(self.last_keyframe_ms) >= self.budget.keyframe_interval_ms;
        let due_bytes = self.bytes_since_keyframe >= self.budget.keyframe_interval_bytes;
        if due_time || due_bytes {
            self.keyframes.push(Keyframe {
                at_ms,
                event_seq: seq,
            });
            self.last_keyframe_ms = at_ms;
            self.bytes_since_keyframe = 0;
        }
    }

    /// Evict oldest events until both budgets are satisfied, then drop any
    /// keyframe whose event fell off the front (its byte range no longer exists).
    /// Never evicts the last remaining event.
    fn evict(&mut self, now_ms: u64) {
        loop {
            if self.events.len() <= 1 {
                break;
            }
            let over_bytes = self.bytes_used > self.budget.max_bytes;
            let front_ms = self.events.front().map(|e| e.at_ms).unwrap_or(0);
            let over_time = self.budget.max_duration_ms > 0
                && now_ms.saturating_sub(front_ms) > self.budget.max_duration_ms;
            if !over_bytes && !over_time {
                break;
            }
            if let Some(ev) = self.events.pop_front() {
                match ev.kind {
                    EventKind::Bytes(b) => {
                        self.bytes_used = self.bytes_used.saturating_sub(b.len() as u64);
                    }
                    // The evicted resize is now the baseline geometry a
                    // reconstruction starts from.
                    EventKind::Resize { rows, cols } => {
                        self.base_rows = rows.max(1);
                        self.base_cols = cols.max(1);
                    }
                }
                self.evicted += 1;
            }
        }
        // Drop keyframes orphaned by eviction (their event is gone).
        if self.evicted > 0 {
            let floor = self.evicted;
            self.keyframes.retain(|k| k.event_seq >= floor);
        }
    }

    /// The earliest retained timestamp (0 when empty).
    pub fn start_ms(&self) -> u64 {
        self.events.front().map(|e| e.at_ms).unwrap_or(0)
    }

    /// The latest retained timestamp — the "live tail" of the recording.
    pub fn end_ms(&self) -> u64 {
        self.events.back().map(|e| e.at_ms).unwrap_or(0)
    }

    /// Whether anything has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Reconstruct the pane's grid at time `at_ms` into a fresh emulator by
    /// re-feeding the retained byte slice. Exact within the retained window.
    pub fn reconstruct(&self, at_ms: u64) -> AlacrittyEmulator {
        let mut emu = AlacrittyEmulator::new(self.base_rows, self.base_cols, REPLAY_SCROLLBACK);
        self.feed_into(&mut emu, 0, at_ms);
        emu
    }

    /// Feed events with `from_exclusive < at_ms <= to_inclusive` into an existing
    /// emulator — the incremental forward-playback path (no full rebuild). Callers
    /// that jump backwards must [`reconstruct`](Self::reconstruct) from scratch.
    pub fn feed_into(&self, emu: &mut AlacrittyEmulator, from_exclusive: u64, to_inclusive: u64) {
        for ev in &self.events {
            if ev.at_ms <= from_exclusive {
                continue;
            }
            if ev.at_ms > to_inclusive {
                break;
            }
            match &ev.kind {
                EventKind::Bytes(b) => emu.advance(b),
                EventKind::Resize { rows, cols } => emu.resize(*rows, *cols),
            }
        }
    }

    /// The next event timestamp strictly after `at_ms` (for single-step scrubbing).
    pub fn next_event_ms(&self, at_ms: u64) -> Option<u64> {
        self.events
            .iter()
            .find(|e| e.at_ms > at_ms)
            .map(|e| e.at_ms)
    }

    /// The previous event timestamp strictly before `at_ms`.
    pub fn prev_event_ms(&self, at_ms: u64) -> Option<u64> {
        self.events
            .iter()
            .rev()
            .find(|e| e.at_ms < at_ms)
            .map(|e| e.at_ms)
    }

    /// If the gap from `at_ms` to the next recorded event exceeds `threshold_ms`,
    /// return that next event's time so playback can skip the idle stretch. `None`
    /// when the gap is small (normal pacing) or there is no later event.
    pub fn skip_target(&self, at_ms: u64, threshold_ms: u64) -> Option<u64> {
        let next = self.next_event_ms(at_ms)?;
        (next.saturating_sub(at_ms) > threshold_ms).then_some(next)
    }

    /// Search across time: the timestamp of the first frame (in the given
    /// direction from `from_ms`) whose reconstructed grid contains `needle`
    /// (case-insensitive). Reconstructs incrementally from the retained front and
    /// tests the styling-agnostic [`grid_text`], so it finds strings that only
    /// ever appeared inside full-screen apps (never in scrollback). Bounded by the
    /// recording budget; `None` if no frame matches.
    pub fn search_next(&self, needle: &str, from_ms: u64, reverse: bool) -> Option<u64> {
        let needle = needle.trim();
        if needle.is_empty() {
            return None;
        }
        let needle = needle.to_lowercase();
        let mut emu = AlacrittyEmulator::new(self.base_rows, self.base_cols, REPLAY_SCROLLBACK);
        let mut best_before: Option<u64> = None;
        for ev in &self.events {
            match &ev.kind {
                EventKind::Bytes(b) => emu.advance(b),
                EventKind::Resize { rows, cols } => emu.resize(*rows, *cols),
            }
            // Sample one frame per event boundary.
            if grid_text(&emu).to_lowercase().contains(&needle) {
                if reverse {
                    if ev.at_ms < from_ms {
                        best_before = Some(ev.at_ms);
                    }
                } else if ev.at_ms > from_ms {
                    return Some(ev.at_ms);
                }
            }
        }
        best_before
    }

    /// Timestamps of the retained keyframes — the scrubber draws a tick at each,
    /// so the timeline shows where activity clustered.
    pub fn keyframe_times(&self) -> impl Iterator<Item = u64> + '_ {
        self.keyframes.iter().map(|k| k.at_ms)
    }
}

/// Flatten an emulator's visible grid to plain text, one row per line, trailing
/// spaces trimmed. Iterates [`PaneEmulator::cell`] directly (reading `.text`
/// regardless of styling) rather than [`PaneEmulator::row_text`], which returns
/// `None` for any styled row — exactly the alt-screen (vim/htop) case that
/// time-search exists to reach.
pub fn grid_text(emu: &dyn PaneEmulator) -> String {
    let (rows, cols) = emu.size();
    let mut out = String::new();
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..cols {
            match emu.cell(row, col) {
                Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                _ => line.push(' '),
            }
        }
        while line.ends_with(' ') {
            line.pop();
        }
        out.push_str(&line);
        if row + 1 < rows {
            out.push('\n');
        }
    }
    out
}

/// Parse a cy-style time expression — a run of `<n><unit>` segments where unit is
/// `d`/`h`/`m`/`s` (e.g. `1h30s`, `3d`, `90s`, `5m`) — into a [`Duration`]. A bare
/// number is treated as seconds. Returns `None` for empty or malformed input, so
/// the search bar can fall back to a literal/regex query.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Bare integer ⇒ seconds.
    if let Ok(secs) = s.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let mut total: u64 = 0;
    let mut num: u64 = 0;
    let mut saw_digit = false;
    let mut saw_unit = false;
    for ch in s.chars() {
        if let Some(d) = ch.to_digit(10) {
            num = num.checked_mul(10)?.checked_add(d as u64)?;
            saw_digit = true;
        } else {
            let mult = match ch {
                'd' => 86_400,
                'h' => 3_600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            if !saw_digit {
                return None; // unit with no preceding number
            }
            total = total.checked_add(num.checked_mul(mult)?)?;
            num = 0;
            saw_digit = false;
            saw_unit = true;
        }
    }
    // Trailing number with no unit is invalid in a mixed expression.
    if saw_digit || !saw_unit {
        return None;
    }
    Some(Duration::from_secs(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ReplayConfig {
        ReplayConfig::default()
    }

    fn small_budget_cfg() -> ReplayConfig {
        ReplayConfig {
            max_bytes_per_pane: 64,
            keyframe_interval_bytes: 16,
            ..ReplayConfig::default()
        }
    }

    #[test]
    fn seek_reconstructs_exact_grid() {
        // Feed a script into a recording and, in lockstep, into a reference
        // emulator; the reconstructed grid at the final time must match.
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let mut reference = AlacrittyEmulator::new(24, 80, REPLAY_SCROLLBACK);
        let epoch = rec.epoch;
        let script: &[&[u8]] = &[b"hello ", b"world\r\n", b"second line\r\n"];
        for (i, chunk) in script.iter().enumerate() {
            let now = epoch + Duration::from_millis(10 * (i as u64 + 1));
            rec.push_bytes(chunk, now);
            reference.advance(chunk);
        }
        let replayed = rec.reconstruct(rec.end_ms());
        assert_eq!(grid_text(&replayed), grid_text(&reference));
        assert!(grid_text(&replayed).contains("hello world"));
        assert!(grid_text(&replayed).contains("second line"));
    }

    #[test]
    fn seek_to_midpoint_omits_later_output() {
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"early\r\n", epoch + Duration::from_millis(10));
        rec.push_bytes(b"late\r\n", epoch + Duration::from_millis(1000));
        let mid = rec.reconstruct(500);
        let text = grid_text(&mid);
        assert!(text.contains("early"));
        assert!(!text.contains("late"));
    }

    #[test]
    fn budget_eviction_drops_oldest_and_orphaned_keyframes() {
        let mut rec = Recording::from_config(&small_budget_cfg(), 24, 80);
        let epoch = rec.epoch;
        // Push well past the 64-byte budget in 16-byte chunks.
        for i in 0..20u64 {
            let chunk = [b'x'; 16];
            rec.push_bytes(&chunk, epoch + Duration::from_millis(i + 1));
        }
        assert!(
            rec.bytes_used <= rec.budget.max_bytes,
            "byte budget enforced"
        );
        assert!(rec.evicted > 0, "some events evicted");
        // Every surviving keyframe must still point at a retained event.
        for k in &rec.keyframes {
            assert!(
                k.event_seq >= rec.evicted,
                "orphaned keyframe survived eviction"
            );
        }
    }

    #[test]
    fn duration_budget_evicts_old_events() {
        let c = ReplayConfig {
            max_duration_secs: 1,
            ..ReplayConfig::default()
        };
        let mut rec = Recording::from_config(&c, 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"old\r\n", epoch + Duration::from_millis(1));
        // 3s later — the 1s window should have evicted the first event.
        rec.push_bytes(b"new\r\n", epoch + Duration::from_millis(3000));
        assert!(rec.evicted >= 1, "stale event evicted by duration budget");
        assert!(rec.start_ms() >= 3000, "front advanced past the window");
    }

    #[test]
    fn resize_replays_at_the_right_time() {
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"before\r\n", epoch + Duration::from_millis(10));
        rec.record_resize(10, 40, epoch + Duration::from_millis(20));
        rec.push_bytes(b"after\r\n", epoch + Duration::from_millis(30));
        let (rows, cols) = rec.reconstruct(rec.end_ms()).size();
        assert_eq!((rows, cols), (10, 40));
    }

    #[test]
    fn search_finds_alt_screen_string_never_in_scrollback() {
        // Simulate a full-screen app: enter the alternate screen, cursor-address
        // and paint a line, then LEAVE the alt screen — so the string is
        // overwritten and never lands in the main-screen scrollback. Scrollback
        // search (row_text) can't find it; time-search must.
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"$ vim\r\n", epoch + Duration::from_millis(10));
        // Enter alt screen (DECSET 1049), home cursor, paint the transient text.
        rec.push_bytes(
            b"\x1b[?1049h\x1b[H-- COMPILE ERROR: needle_xyz --",
            epoch + Duration::from_millis(20),
        );
        // Leave the alt screen (DECRST 1049) — the transient frame is gone.
        rec.push_bytes(b"\x1b[?1049l$ \r\n", epoch + Duration::from_millis(30));

        // The final live grid must NOT contain the string …
        let live = rec.reconstruct(rec.end_ms());
        assert!(
            !grid_text(&live).contains("needle_xyz"),
            "string should be gone from the live grid"
        );
        // … but time-search finds the frame where it appeared.
        let hit = rec.search_next("needle_xyz", 0, false);
        assert_eq!(hit, Some(20), "time-search locates the alt-screen frame");

        // Seeking there reconstructs a grid that shows it.
        let at = rec.reconstruct(hit.unwrap());
        assert!(grid_text(&at).contains("needle_xyz"));
    }

    #[test]
    fn search_reverse_finds_earlier_match() {
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"alpha\r\n", epoch + Duration::from_millis(10));
        rec.push_bytes(b"beta\r\n", epoch + Duration::from_millis(20));
        // Reverse search from *between* the two events finds the latest prior
        // match — the frame at t=10 (the t=20 frame is after the cursor).
        assert_eq!(rec.search_next("alpha", 15, true), Some(10));
        // Forward search from 0 finds the first frame containing "beta".
        assert_eq!(rec.search_next("beta", 0, false), Some(20));
        // Reverse from the end returns the most recent matching frame.
        assert_eq!(rec.search_next("beta", 1000, true), Some(20));
        // Missing needle ⇒ None.
        assert_eq!(rec.search_next("gamma", 0, false), None);
    }

    #[test]
    fn skip_target_collapses_only_large_gaps() {
        let mut rec = Recording::from_config(&cfg(), 24, 80);
        let epoch = rec.epoch;
        rec.push_bytes(b"a", epoch + Duration::from_millis(100));
        rec.push_bytes(b"b", epoch + Duration::from_millis(5000)); // 4.9s idle gap
        // A 1s threshold ⇒ the gap after t=100 collapses to the next event (5000).
        assert_eq!(rec.skip_target(100, 1000), Some(5000));
        // From t=5000 there's no later event.
        assert_eq!(rec.skip_target(5000, 1000), None);
    }

    #[test]
    fn parse_duration_table() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h30s"), Some(Duration::from_secs(3630)));
        assert_eq!(parse_duration("3d"), Some(Duration::from_secs(259_200)));
        assert_eq!(parse_duration("2h15m"), Some(Duration::from_secs(8100)));
        assert_eq!(parse_duration("42"), Some(Duration::from_secs(42)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("10x"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("1h30"), None); // trailing unitless number
    }

    #[test]
    fn disabled_config_is_the_callers_null_check() {
        // Documents the contract: when disabled the caller holds `None` and never
        // constructs a Recording. Here we just assert the default is enabled.
        assert!(ReplayConfig::default().enabled);
    }
}
