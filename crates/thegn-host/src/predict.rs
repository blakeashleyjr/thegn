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
    /// Character typed at the first un-echoed key in this burst.
    key_char: Option<char>,
    /// Row on which server output confirmed that an earlier key was echoed.
    confirmed_row: Option<usize>,
}

/// Latency floor (ms) below which local echo is imperceptible, so we don't
/// bother predicting (avoids any glitch risk on a fast/local link).
pub const PREDICT_MIN_SRTT_MS: f64 = 50.0;
pub const PREDICT_STALE_MS: u64 = 2000;

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
    pub fn note_key(&mut self, now_ms: u64, cursor: (usize, usize), c: char) {
        if self.last_key_ms.is_none() {
            self.last_key_ms = Some(now_ms);
        }
        if self.key_cursor.is_none() {
            self.key_cursor = Some(cursor);
            self.key_char = Some(c);
        }
    }

    /// A printable keystroke was sent AND we're predicting: note it (srtt) and
    /// add it to the overlay. Caller has already checked `should_predict`.
    pub fn on_key(&mut self, c: char, now_ms: u64, cursor: (usize, usize)) {
        self.note_key(now_ms, cursor, c);
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

    /// Real server output arrived: reconcile the overlay against the cells the
    /// authoritative emulator now contains. A cursor that did not move is not
    /// evidence that the server rejected the input, so keep the overlay until
    /// it either advances, mismatches, or goes stale.
    pub fn on_server_output(
        &mut self,
        now_ms: u64,
        cursor: (usize, usize),
        cell: &dyn Fn(usize, usize) -> Option<String>,
    ) {
        let Some((row, col)) = self.key_cursor else {
            if let Some(start) = self.last_key_ms.take() {
                self.srtt.observe(now_ms.saturating_sub(start) as f64);
            }
            self.confirmed_row = None;
            self.pending.clear();
            self.key_char = None;
            return;
        };

        if cursor == (row, col) {
            if self
                .last_key_ms
                .is_some_and(|start| now_ms.saturating_sub(start) > PREDICT_STALE_MS)
            {
                self.clear();
            }
            return;
        }

        let advanced_same_row = cursor.0 == row && cursor.1 > col;
        let pending_is_ascii = self.pending.iter().all(|c| c.is_ascii());
        let n = advanced_same_row
            .then(|| (cursor.1 - col).min(self.pending.len().max(1)))
            .unwrap_or(0);
        let all_match = advanced_same_row
            && pending_is_ascii
            && (0..n).all(|i| {
                let expected = self.pending.get(i).copied().or(self.key_char);
                let actual = cell(row, col + i).and_then(|text| text.chars().next());
                expected.is_some_and(|expected| actual == Some(expected))
            });

        if all_match {
            if let Some(start) = self.last_key_ms.take() {
                self.srtt.observe(now_ms.saturating_sub(start) as f64);
            }
            self.confirmed_row = Some(row);
            self.pending.drain(..n.min(self.pending.len()));
            if self.pending.is_empty() {
                self.key_cursor = None;
                self.key_char = None;
            } else {
                self.key_cursor = Some((row, col + n));
                self.key_char = self.pending.first().copied();
                self.last_key_ms = Some(now_ms);
            }
        } else {
            if let Some(start) = self.last_key_ms.take() {
                self.srtt.observe(now_ms.saturating_sub(start) as f64);
            }
            self.confirmed_row = None;
            self.pending.clear();
            self.key_cursor = None;
            self.key_char = None;
        }
    }

    /// Drop predictions without taking an srtt sample (resize, focus loss, …).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.last_key_ms = None;
        self.key_cursor = None;
        self.key_char = None;
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

    fn prompt(cursor_row: usize, cursor_col: usize, cols: usize) -> ScreenState {
        ScreenState {
            alt_screen: false,
            app_mode: false,
            cursor_row,
            cursor_col,
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
        p.on_server_output(420, (0, 1), &|_, _| None); // 320 ms round-trip
        assert!(p.is_empty());
        assert_eq!(p.srtt_ms(), Some(320.0));
    }

    #[test]
    fn gate_blocks_fullscreen_rawmode_no_echo_and_fast_links() {
        let mut p = Predictor::new();
        // No srtt yet ⇒ don't predict.
        assert!(!p.should_predict(&prompt(23, 0, 80)));
        // Seed a slow link.
        p.on_key('a', 0, (23, 0));
        p.on_server_output(320, (23, 1), &|_, _| Some("a".into()));
        assert!(
            p.should_predict(&prompt(23, 1, 80)),
            "slow link, prompt row ⇒ predict"
        );
        // Full-screen / raw apps never predict.
        assert!(!p.should_predict(&ScreenState {
            alt_screen: true,
            ..prompt(23, 1, 80)
        }));
        assert!(!p.should_predict(&ScreenState {
            app_mode: true,
            ..prompt(23, 1, 80)
        }));
        // A server response that did not advance the cursor is not echo.
        p.on_key('p', 400, (10, 4));
        p.on_server_output(720, (10, 4), &|_, _| None);
        assert!(!p.should_predict(&prompt(10, 4, 80)));
    }

    #[test]
    fn fast_link_does_not_predict() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (23, 0));
        p.on_server_output(8, (23, 1), &|_, _| None); // 8 ms — local/fast
        assert!(!p.should_predict(&prompt(23, 1, 80)));
    }

    #[test]
    fn confirmed_row_predicts_mid_screen() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("a".into()));
        assert!(p.should_predict(&prompt(10, 5, 80)));
    }

    #[test]
    fn right_edge_guard_accounts_for_pending_chars() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("a".into()));
        p.on_key('b', 321, (10, 5));
        assert!(!p.should_predict(&prompt(10, 78, 80)));
    }

    #[test]
    fn enter_resets_confirmation() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("a".into()));
        assert!(p.should_predict(&prompt(10, 5, 80)));
        p.on_enter();
        assert!(!p.should_predict(&prompt(10, 5, 80)));
    }

    #[test]
    fn partial_echo_retires_one_key_and_keeps_predicting() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("a".into()));
        p.on_key('b', 321, (10, 5));
        p.on_key('c', 322, (10, 6));
        p.on_server_output(640, (10, 6), &|row, col| {
            ((row, col) == (10, 5)).then(|| "b".into())
        });
        assert_eq!(p.pending(), &['c']);
        assert!(p.should_predict(&prompt(10, 6, 80)));
    }

    #[test]
    fn mismatched_echo_clears_all_predictions() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_key('b', 1, (10, 5));
        p.on_server_output(320, (10, 6), &|_, _| Some("x".into()));
        assert!(p.is_empty());
        assert_eq!(p.confirmed_row, None);
    }

    #[test]
    fn no_op_output_keeps_overlay() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("a".into()));
        p.on_key('b', 400, (10, 5));
        p.on_server_output(500, (10, 5), &|_, _| None);
        assert_eq!(p.pending(), &['b']);
        assert_eq!(p.confirmed_row, Some(10));
        assert_eq!(p.srtt_ms(), Some(320.0));
    }

    #[test]
    fn stale_no_op_output_clears_overlay() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_server_output(PREDICT_STALE_MS + 1, (10, 4), &|_, _| None);
        assert!(p.is_empty());
        assert_eq!(p.key_cursor, None);
        assert_eq!(p.srtt_ms(), None);
    }

    #[test]
    fn non_ascii_pending_char_clears_overlay() {
        let mut p = Predictor::new();
        p.on_key('é', 0, (10, 4));
        p.on_server_output(320, (10, 5), &|_, _| Some("é".into()));
        assert!(p.is_empty());
        assert_eq!(p.confirmed_row, None);
    }

    #[test]
    fn full_echo_leaves_confirmed_row_after_emptying_overlay() {
        let mut p = Predictor::new();
        p.on_key('a', 0, (10, 4));
        p.on_key('b', 1, (10, 5));
        p.on_server_output(320, (10, 6), &|row, col| match (row, col) {
            (10, 4) => Some("a".into()),
            (10, 5) => Some("b".into()),
            _ => None,
        });
        assert!(p.is_empty());
        assert_eq!(p.confirmed_row, Some(10));
    }

    #[test]
    fn mask_echo_never_confirms() {
        let mut p = Predictor::new();
        p.on_key('p', 0, (5, 10));
        p.on_server_output(320, (5, 11), &|_, _| Some("*".into()));
        assert!(!p.should_predict(&prompt(5, 11, 80)));
    }
}
