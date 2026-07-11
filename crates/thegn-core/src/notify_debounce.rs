//! Burst suppression for repeat notifications: a flaky remote pane that
//! crash-respawns emits an identical `process_failed` every few seconds — one
//! alert is signal, ten are noise. Pure sliding-window dedup keyed
//! `(worktree, kind)`; the host's notify chokepoint consults it before
//! recording.

use std::collections::HashMap;

/// Sliding-window notification dedup. First emission in a window passes;
/// identical `(worktree, kind)` pairs within `window_secs` are suppressed.
#[derive(Debug)]
pub struct NotifyDebounce {
    window_secs: i64,
    last: HashMap<(String, String), i64>,
}

impl Default for NotifyDebounce {
    fn default() -> Self {
        NotifyDebounce::new(60)
    }
}

impl NotifyDebounce {
    pub fn new(window_secs: i64) -> Self {
        NotifyDebounce {
            window_secs,
            last: HashMap::new(),
        }
    }

    /// Should a `(worktree, kind)` notification at unix time `now` emit?
    /// Advances the window on emission; suppressed repeats do NOT extend it,
    /// so a steady failure re-alerts once per window rather than never.
    pub fn allow(&mut self, worktree: &str, kind: &str, now: i64) -> bool {
        let key = (worktree.to_string(), kind.to_string());
        match self.last.get(&key) {
            Some(&t) if now.saturating_sub(t) < self.window_secs => false,
            _ => {
                self.last.insert(key, now);
                // Opportunistic cleanup: drop long-expired entries so a
                // session that touches many worktrees doesn't accrete.
                if self.last.len() > 256 {
                    let w = self.window_secs;
                    self.last.retain(|_, t| now.saturating_sub(*t) < w * 4);
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_repeats_within_window_then_realerts() {
        let mut d = NotifyDebounce::new(60);
        assert!(d.allow("/wt/a", "process_failed", 1000), "first passes");
        assert!(
            !d.allow("/wt/a", "process_failed", 1005),
            "burst suppressed"
        );
        assert!(!d.allow("/wt/a", "process_failed", 1059), "still inside");
        assert!(d.allow("/wt/a", "process_failed", 1060), "window re-opens");
        // Suppressed repeats did not extend the window (re-alert happened at
        // 1060, not 1059+60).
    }

    #[test]
    fn distinct_keys_do_not_interfere() {
        let mut d = NotifyDebounce::new(60);
        assert!(d.allow("/wt/a", "process_failed", 1000));
        assert!(d.allow("/wt/b", "process_failed", 1001), "other worktree");
        assert!(d.allow("/wt/a", "agent_failed", 1002), "other kind");
    }

    #[test]
    fn cleanup_keeps_the_map_bounded() {
        let mut d = NotifyDebounce::new(60);
        for i in 0..400 {
            // Spread far apart in time so old entries age out at cleanup.
            assert!(d.allow(&format!("/wt/{i}"), "process_failed", i * 1000));
        }
        assert!(
            d.last.len() <= 257,
            "opportunistic cleanup ran: {}",
            d.last.len()
        );
    }
}
