//! Predictive local echo for high-latency remote panes (the mosh idea, scoped).
//!
//! A sprite pane round-trips every keystroke ~one RTT (~320 ms) before it echoes,
//! and the lag is the RTT, so no transport (WSS exec / ssh-over-WSS) fixes it —
//! only echoing keystrokes LOCALLY and reconciling when the server's authoritative
//! output lands. thegn owns the emulator, so it can: on a printable keystroke,
//! append to a *prediction overlay* + advance a *predicted cursor*, render it
//! immediately (dimmed) at the prompt, and DROP the overlay the moment real server
//! output arrives (the server bytes already carry the echoed text / correct state).
//! Password prompts and other no-echo input never predict: a burst is eligible
//! only after server output confirms that an earlier key advanced the cursor.
//!
//! This module is the **pure, substrate-free half**: the prediction state machine,
//! a smoothed round-trip estimator, and the safety-gate decision. The pane wires
//! keystrokes/output/render to it (see the `predict` design doc). Everything here
//! is deterministic + unit-tested; time is injected (millis) so there's no clock.
//
// The pane integration is live: `PtyPane` drives the keystroke/output hooks and
// the alt-screen/app-mode/prompt-row gates (see `pane.rs`), and `run.rs` paints
// the overlay + advances the caret just before the frame diff. A couple of
// diagnostic accessors (srtt_ms, clear) are kept for tuning.
#![allow(dead_code)]

/// A smoothed round-trip-time estimate (EWMA), in milliseconds. Drives the
/// latency gate: predicting only helps once the round-trip is actually slow.
#[derive(Debug, Clone)]
pub struct Srtt {
    ms: Option<f64>,
    alpha: f64,
}

impl Default for Srtt {
    fn default() -> Self {
        // 0.25 — standard-ish EWMA weight on the newest sample.
        Srtt {
            ms: None,
            alpha: 0.25,
        }
    }
}

impl Srtt {
    /// Fold a fresh round-trip sample (the gap from a keystroke to the server
    /// output that echoed it). Ignores absurd samples so a paused tab / a slow
    /// command's output doesn't poison the estimate.
    pub fn observe(&mut self, sample_ms: f64) {
        if !(0.0..=10_000.0).contains(&sample_ms) {
            return;
        }
        self.ms = Some(match self.ms {
            None => sample_ms,
            Some(prev) => prev * (1.0 - self.alpha) + sample_ms * self.alpha,
        });
    }
    /// Current estimate, or `None` until the first sample.
    pub fn get(&self) -> Option<f64> {
        self.ms
    }
}

/// What the emulator looks like right now — the inputs to the safety gate.
/// Predicting is only safe at a normal shell prompt; full-screen / raw apps
/// (vim, htop, fzf, …) manage their own echo, so predicting there corrupts the
/// display.
#[derive(Debug, Clone, Copy)]
pub struct ScreenState {
    /// Alternate screen active (a full-screen TUI) — never predict.
    pub alt_screen: bool,
    /// Application cursor-key / raw mode — never predict.
    pub app_mode: bool,
    /// The cursor's row and column in the grid.
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub rows: usize,
    pub cols: usize,
}

/// Per-pane predictive-echo state.
#[derive(Debug, Clone, Default)]
pub struct Predictor {
    /// Printable chars typed since the last server output, not yet confirmed.
    pending: Vec<char>,
    srtt: Srtt,
    /// Timestamp (ms) of the first un-echoed keystroke in this burst, for srtt.
    last_key_ms: Option<u64>,
    /// Cursor at the first un-echoed key in this burst.
    key_cursor: Option<(usize, usize)>,
    /// Row on which server output confirmed that an earlier key was echoed.
    confirmed_row: Option<usize>,
}

/// Latency floor (ms) below which local echo is imperceptible, so we don't
/// bother predicting (avoids any glitch risk on a fast/local link).
pub const PREDICT_MIN_SRTT_MS: f64 = 50.0;

impl Predictor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The chars to render (dimmed) at the cursor right now.
    pub fn pending(&self) -> &[char] {
        &self.pending
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Should we locally echo, given the screen state + measured latency? Gates
    /// out full-screen/raw apps, no-echo prompts, right-edge cursors, and fast
    /// links.
    pub fn should_predict(&self, s: &ScreenState) -> bool {
        if s.alt_screen || s.app_mode {
            return false;
        }
        if self.confirmed_row != Some(s.cursor_row) {
            return false;
        }
        // Only once the link is actually slow enough to feel.
        if !self.srtt.get().is_some_and(|ms| ms >= PREDICT_MIN_SRTT_MS) {
            return false;
        }
        // Leave room for the next predicted character; never paint past the
        // right edge of the terminal.
        s.cursor_col
            .saturating_add(self.pending.len())
            .saturating_add(1)
            < s.cols
    }

    /// Record that a keystroke was sent (for the srtt estimate), WITHOUT
    /// predicting. Called on every keystroke so srtt builds even before the
    /// latency gate opens — otherwise `should_predict` (which needs srtt) and
    /// the srtt (which needs keystroke timing) would deadlock. Stamps only the
    /// first un-echoed keystroke of a burst.
    pub fn note_key(&mut self, now_ms: u64, cursor: (usize, usize)) {
        if self.last_key_ms.is_none() {
            self.last_key_ms = Some(now_ms);
        }
        if self.key_cursor.is_none() {
            self.key_cursor = Some(cursor);
        }
    }

    /// A printable keystroke was sent AND we're predicting: note it (srtt) and
    /// add it to the overlay. Caller has already checked `should_predict`.
    pub fn on_key(&mut self, c: char, now_ms: u64, cursor: (usize, usize)) {
        self.note_key(now_ms, cursor);
        self.pending.push(c);
    }

    /// Backspace — drop the last prediction (no-op if none pending).
    pub fn on_backspace(&mut self) {
        self.pending.pop();
    }

    /// A line was submitted (Enter) — flush predictions; the server will redraw.
    pub fn on_enter(&mut self) {
        self.clear();
    }

    /// Real server output arrived: it's authoritative (and carries the echoed
    /// text), so drop the overlay. Also folds a round-trip sample into the srtt
    /// when this output plausibly echoes the pending burst.
    pub fn on_server_output(&mut self, now_ms: u64, cursor: (usize, usize)) {
        if let Some(start) = self.last_key_ms.take() {
            self.srtt.observe(now_ms.saturating_sub(start) as f64);
        }
        self.confirmed_row = match self.key_cursor {
            Some((row, col)) if cursor.0 == row && cursor.1 > col => Some(row),
            _ => None,
        };
        self.pending.clear();
        self.key_cursor = None;
    }

    /// Drop predictions without taking an srtt sample (resize, focus loss, …).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.last_key_ms = None;
        self.key_cursor = None;
        self.confirmed_row = None;
    }

    /// Current smoothed RTT estimate (ms), for diagnostics.
    pub fn srtt_ms(&self) -> Option<f64> {
        self.srtt.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(rows: usize, cursor_row: usize, cursor_col: usize, cols: usize) -> ScreenState {
        ScreenState {
            alt_screen: false,
            app_mode: false,
            cursor_row,
            cursor_col,
            rows,
            cols,
        }
    }

    #[test]
    fn srtt_ewma_smooths_and_rejects_outliers() {
        let mut s = Srtt::default();
        s.observe(300.0);
        assert_eq!(s.get(), Some(300.0)); // first sample seeds
        s.observe(320.0);
        assert!((s.get().unwrap() - 305.0).abs() < 0.01); // 300*.75 + 320*.25
        s.observe(999_999.0); // absurd (a slow command) — ignored
        assert!((s.get().unwrap() - 305.0).abs() < 0.01);
    }

    #[test]
    fn key_backspace_enter_and_output_manage_the_overlay() {
        let mut p = Predictor::new();
        p.on_key('l', 0, (0, 0));
        p.on_key('s', 1, (0, 1));
        assert_eq!(p.pending(), &['l', 's']);
        p.on_backspace();
        assert_eq!(p.pending(), &['l']);
        // Enter submits the line → overlay flushes.
        p.on_enter();
        assert!(p.is_empty());
        // A fresh burst, then server output clears it + records the rtt.
        p.on_key('x', 100, (0, 0));
        p.on_server_output(420, (0, 1)); // 320 ms round-trip
        assert!(p.is_empty());
        assert_eq!(p.srtt_ms(), Some(320.0));
    }

    #[test]
    fn gate_blocks_fullscreen_rawmode_no_echo_and_fast_links() {
        let mut p = Predictor::new();
        // No srtt yet ⇒ don't predict.
        assert!(!p.should_predict(&prompt(24, 23, 0, 80)));
        // Seed a slow link.
        p.on_key('a', 0, (23, 0));
        p.on_server_output(320, (23, 1));
        assert!(
            p.should_predict(&prompt(24, 23, 1, 80)),
            "slow link, prompt row ⇒ predict"
        );
        // Full-screen / raw apps never predict.
        assert!(!p.should_predict(&ScreenState {
            alt_screen: true,
            ..prompt(24, 23, 1, 80)
        }));
        assert!(!p.should_predict(&ScreenState {
            app_mode: true,
            ..prompt(24, 23, 1, 80)
        }));
        // A server response that did not advance the cursor is not echo.
        p.on_key('p', 400, (10, 4));
        p.on_server_output(720, (10, 4));
        assert!(!p.should_predict(&prompt(24, 10, 4, 80)));
    }

    #[test]
    fn fast_link_does_not_predict() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (23, 0));
        p.on_server_output(8, (23, 1)); // 8 ms — local/fast
        assert!(!p.should_predict(&prompt(24, 23, 1, 80)));
    }

    #[test]
    fn confirmed_row_predicts_mid_screen() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5));
        assert!(p.should_predict(&prompt(24, 10, 5, 80)));
    }

    #[test]
    fn right_edge_guard_accounts_for_pending_chars() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5));
        p.on_key('b', 321, (10, 5));
        assert!(!p.should_predict(&prompt(24, 10, 78, 80)));
    }

    #[test]
    fn enter_resets_confirmation() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5));
        assert!(p.should_predict(&prompt(24, 10, 5, 80)));
        p.on_enter();
        assert!(!p.should_predict(&prompt(24, 10, 5, 80)));
    }
}
