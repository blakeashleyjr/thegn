//! Lifecycle of the bottom-bar status message (`FrameModel::status`).
//!
//! The status slot is **loop-owned**: hydration never seeds it and the mode
//! chip only *replaces* it when a non-default input mode is entered. Every
//! other writer (`model.status = …` after a user action, a crash alert, a
//! config error) gets a fixed lifetime — [`STATUS_TTL`] — after which the slot
//! reverts to the mode text (empty in `Normal`). Expiry is driven by a one-shot
//! waker pulse scheduled when the message lands, so an idle loop (which never
//! polls on a timer) still clears it on time.
//!
//! Before this tracker existed the slot had no lifetime at all *and* was wiped
//! by every hydration swap (`build_model` seeded a startup line that defeated
//! the "restore if empty" guard, and `apply_mode_status` then blanked it), so a
//! message lived anywhere between 0 ms and the next 5 s tick. Messages posted
//! in the same iteration as a hydration (`"rebase finished"`) never rendered.

use std::time::{Duration, Instant};

use crate::chrome::FrameModel;
use crate::keymap::Mode;

/// How long a transient status message stays on the bar.
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(8);

/// The status text a mode contributes when nothing else is showing.
pub(crate) fn mode_text(mode: Mode) -> String {
    match mode {
        Mode::Normal => String::new(),
        m => format!("{} mode", m.as_str()),
    }
}

/// True when `s` is one of the mode strings (so it must not be subject to the
/// TTL and may be replaced on a mode switch).
pub(crate) fn is_mode_text(s: &str) -> bool {
    s.is_empty()
        || [Mode::VimNormal, Mode::VimInsert, Mode::Emacs]
            .iter()
            .any(|m| mode_text(*m) == s)
}

/// Reflect an input-mode change in the status slot. A transient message that
/// is currently showing is left alone — the mode chip is always visible, and
/// the message will revert to the mode text when its TTL lapses.
pub(crate) fn apply_mode(model: &mut FrameModel, mode: Mode) {
    if is_mode_text(&model.status) {
        model.status = mode_text(mode);
    }
}

/// Loop-local tracker; call [`StatusLine::tick`] once per iteration, right
/// before rendering.
#[derive(Debug, Default)]
pub(crate) struct StatusLine {
    last: String,
    since: Option<Instant>,
}

impl StatusLine {
    /// Observe the current status. Returns `true` when the tracker changed the
    /// model (an expired message was cleared) and a repaint is needed.
    ///
    /// `schedule` is handed the delay after which the loop must wake to
    /// re-run this tick; it is invoked at most once per new message.
    pub(crate) fn tick(
        &mut self,
        model: &mut FrameModel,
        mode: Mode,
        now: Instant,
        mut schedule: impl FnMut(Duration),
    ) -> bool {
        if model.status != self.last {
            // A new message landed this iteration: start its clock.
            self.last = model.status.clone();
            if is_mode_text(&model.status) {
                self.since = None;
            } else {
                self.since = Some(now);
                schedule(STATUS_TTL + Duration::from_millis(50));
            }
            return false;
        }
        match self.since {
            Some(t0) if now.duration_since(t0) >= STATUS_TTL => {
                model.status = mode_text(mode);
                self.last = model.status.clone();
                self.since = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(status: &str) -> FrameModel {
        FrameModel {
            status: status.into(),
            ..FrameModel::default()
        }
    }

    #[test]
    fn message_survives_until_ttl_then_reverts_to_mode_text() {
        let mut sl = StatusLine::default();
        let mut m = model_with("Copied log line");
        let t0 = Instant::now();
        let mut scheduled = Vec::new();
        assert!(!sl.tick(&mut m, Mode::Normal, t0, |d| scheduled.push(d)));
        assert_eq!(scheduled.len(), 1, "one wake scheduled per message");
        // Half-way: untouched, no extra wake.
        assert!(!sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL / 2, |d| {
            scheduled.push(d)
        }));
        assert_eq!(m.status, "Copied log line");
        assert_eq!(scheduled.len(), 1);
        // Past TTL: cleared, repaint requested.
        assert!(sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL, |d| scheduled.push(d)));
        assert_eq!(m.status, "");
    }

    #[test]
    fn expiry_reverts_to_the_mode_string_not_empty() {
        let mut sl = StatusLine::default();
        let mut m = model_with("rebase finished");
        let t0 = Instant::now();
        sl.tick(&mut m, Mode::VimNormal, t0, |_| {});
        assert!(sl.tick(&mut m, Mode::VimNormal, t0 + STATUS_TTL, |_| {}));
        assert_eq!(m.status, "VimNormal mode");
    }

    #[test]
    fn mode_text_has_no_ttl() {
        let mut sl = StatusLine::default();
        let mut m = model_with("Emacs mode");
        let t0 = Instant::now();
        let mut n = 0;
        sl.tick(&mut m, Mode::Emacs, t0, |_| n += 1);
        assert_eq!(n, 0, "mode text must not schedule a wake");
        assert!(!sl.tick(&mut m, Mode::Emacs, t0 + STATUS_TTL * 10, |_| {}));
        assert_eq!(m.status, "Emacs mode");
    }

    #[test]
    fn a_newer_message_restarts_the_clock() {
        let mut sl = StatusLine::default();
        let mut m = model_with("first");
        let t0 = Instant::now();
        sl.tick(&mut m, Mode::Normal, t0, |_| {});
        m.status = "second".into();
        sl.tick(
            &mut m,
            Mode::Normal,
            t0 + STATUS_TTL - Duration::from_secs(1),
            |_| {},
        );
        // The first message's deadline passes; the second is still young.
        assert!(!sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL, |_| {}));
        assert_eq!(m.status, "second");
    }

    #[test]
    fn apply_mode_does_not_clobber_a_live_message() {
        let mut m = model_with("Config error: bad toml");
        apply_mode(&mut m, Mode::Normal);
        assert_eq!(m.status, "Config error: bad toml");
        apply_mode(&mut m, Mode::VimNormal);
        assert_eq!(m.status, "Config error: bad toml");
        let mut m = model_with("VimNormal mode");
        apply_mode(&mut m, Mode::Normal);
        assert_eq!(m.status, "");
        apply_mode(&mut m, Mode::Emacs);
        assert_eq!(m.status, "Emacs mode");
    }
}
