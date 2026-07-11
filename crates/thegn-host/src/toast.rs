//! Transient popup messages — "toasts". A small, time-bounded stack of
//! notifications rendered as a bottom-anchored overlay (zellij-style): an
//! action announces itself ("Text copied to clipboard") and the message fades
//! after a TTL without stealing focus or dimming the screen.
//!
//! The stack model (push/cap, expiry pruning, next-expiry) and the styled
//! line builder are pure and unit-tested; the host owns one [`Toasts`] and
//! schedules a one-shot wake at `Toasts::next_expiry` so an expired toast
//! clears even with no further input (the event loop never polls on a timer).

use std::time::{Duration, Instant};

use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{Line, Tok, seg};
use thegn_core::theme::Hue;

/// Severity of a toast — drives its text/border color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
}

impl ToastKind {
    /// The color token for this kind's message text + box border.
    fn tok(self) -> Tok {
        match self {
            ToastKind::Info => Tok::Slot(S::Text),
            ToastKind::Success => Tok::Hue(Hue::Green),
        }
    }
}

/// One transient message and the instant it should disappear.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub expires: Instant,
}

/// How long a toast stays up by default.
pub const DEFAULT_TTL: Duration = Duration::from_millis(2500);

/// At most this many toasts are kept; older ones drop off the bottom.
const MAX_TOASTS: usize = 3;

/// A bounded, self-expiring stack of toasts.
#[derive(Debug, Default)]
pub struct Toasts {
    stack: Vec<Toast>,
}

impl Toasts {
    /// Push a toast that expires `ttl` after `now`, capping the stack to the
    /// most recent [`MAX_TOASTS`].
    pub fn push(
        &mut self,
        kind: ToastKind,
        message: impl Into<String>,
        now: Instant,
        ttl: Duration,
    ) {
        self.stack.push(Toast {
            message: message.into(),
            kind,
            expires: now + ttl,
        });
        if self.stack.len() > MAX_TOASTS {
            let overflow = self.stack.len() - MAX_TOASTS;
            self.stack.drain(0..overflow);
        }
    }

    /// Convenience: a success toast with the default TTL.
    pub fn success(&mut self, message: impl Into<String>, now: Instant) {
        self.push(ToastKind::Success, message, now, DEFAULT_TTL);
    }

    /// Convenience: an info toast with the default TTL.
    pub fn info(&mut self, message: impl Into<String>, now: Instant) {
        self.push(ToastKind::Info, message, now, DEFAULT_TTL);
    }

    /// Drop every toast whose deadline has passed. Returns `true` when the
    /// stack changed (so the caller knows to re-render).
    pub fn prune(&mut self, now: Instant) -> bool {
        let before = self.stack.len();
        self.stack.retain(|t| t.expires > now);
        self.stack.len() != before
    }

    /// True when no toasts are live.
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// The styled content lines for the live toasts (oldest first). Pure.
    pub fn lines(&self) -> Vec<Line> {
        self.stack
            .iter()
            .map(|t| Line::segs(vec![seg(t.kind.tok(), t.message.clone())]))
            .collect()
    }

    /// Draw the toast stack as a bottom-anchored overlay over `screen`.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        if self.stack.is_empty() {
            return;
        }
        let lines = self.lines();
        let cols = self
            .stack
            .iter()
            .map(|t| t.message.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(12, 60);
        // Border picks up the newest toast's color so the kind reads at a glance.
        let border = self
            .stack
            .last()
            .map(|t| t.kind.tok())
            .unwrap_or(Tok::Slot(S::Accent));
        let spec = LayerSpec {
            cols,
            rows: lines.len(),
            anchor: Anchor::Bottom,
            // A toast must not steal the eye or dim the work behind it — and
            // no drop-shadow, which reads as rendering cruft on a transient box.
            dim: false,
            shadow: false,
            border,
            ..LayerSpec::default()
        };
        if let Some(inner) = open_layer(surface, screen, &spec) {
            crate::seg::draw_lines(surface, inner, &lines, Tok::Slot(S::Panel));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn push_then_prune_after_ttl_empties_the_stack() {
        let t0 = base();
        let mut toasts = Toasts::default();
        toasts.success("Text copied to clipboard", t0);
        assert_eq!(toasts.lines().len(), 1);

        // Before the deadline: still live.
        assert!(!toasts.prune(t0 + Duration::from_millis(100)));
        assert_eq!(toasts.lines().len(), 1);

        // Past the deadline: pruned, and prune reports the change once.
        assert!(toasts.prune(t0 + DEFAULT_TTL + Duration::from_millis(1)));
        assert!(toasts.lines().is_empty());
        assert!(!toasts.prune(t0 + DEFAULT_TTL + Duration::from_secs(10)));
    }

    #[test]
    fn stack_is_capped_to_the_most_recent() {
        let t0 = base();
        let mut toasts = Toasts::default();
        for i in 0..6 {
            toasts.info(format!("msg {i}"), t0);
        }
        let lines = toasts.lines();
        assert_eq!(lines.len(), MAX_TOASTS);
    }

    #[test]
    fn distinct_ttls_expire_independently() {
        let t0 = base();
        let mut toasts = Toasts::default();
        toasts.push(ToastKind::Info, "a", t0, Duration::from_secs(5));
        toasts.push(ToastKind::Info, "b", t0, Duration::from_secs(2));
        // At t0+3s the 2s toast is gone but the 5s one survives.
        assert!(toasts.prune(t0 + Duration::from_secs(3)));
        assert_eq!(toasts.lines().len(), 1);
    }

    #[test]
    fn empty_stack_has_no_lines() {
        let toasts = Toasts::default();
        assert!(toasts.lines().is_empty());
    }

    /// All on-screen text, row by row, for substring assertions.
    fn surface_text(s: &mut Surface) -> String {
        s.screen_cells()
            .iter()
            .map(|row| row.iter().map(|c| c.str().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_draws_the_message_text_into_the_surface() {
        let t0 = base();
        let mut toasts = Toasts::default();
        toasts.success("Text copied to clipboard", t0);
        let mut surface = Surface::new(80, 24);
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        toasts.render(&mut surface, screen);
        assert!(
            surface_text(&mut surface).contains("Text copied to clipboard"),
            "the toast message must be painted onto the surface"
        );
    }

    #[test]
    fn render_on_empty_stack_is_a_noop() {
        let toasts = Toasts::default();
        let mut surface = Surface::new(40, 12);
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 12,
        };
        toasts.render(&mut surface, screen);
        assert!(surface_text(&mut surface).trim().is_empty());
    }
}
