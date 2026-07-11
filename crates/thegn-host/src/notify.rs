//! Notification dispatch chokepoint (items 420/426/427/429).
//!
//! [`NotifyState`] is the shared runtime handle threaded to every place that
//! emits a notification. It holds the effective `[notifications]` config, the
//! runtime DND toggle + active routing mode, and the terminal bell latch. Emit
//! sites call [`NotifyState::decide`] once and then honor the returned
//! [`RouteDecision`]: record to the inbox (unless dropped), pop a desktop toast
//! (unless suppressed), and ring the sound (bell latched for the render loop, a
//! command spawned off-thread).
//!
//! The routing logic itself is the pure `thegn_core::notification_route`
//! engine; this module only supplies the clock + runtime state and performs the
//! I/O the decision authorizes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::NotificationsConfig;
use thegn_core::notification::NotificationKind;
use thegn_core::notification_route::{RouteCtx, RouteDecision, SoundEmit, decide};
use thegn_core::store::NotificationStore;

/// Shared, thread-safe notification runtime. Cloned (as `Arc`) into the
/// background dispatch closures and read by the event loop.
pub struct NotifyState {
    cfg: Mutex<NotificationsConfig>,
    /// Burst suppression for repeat process-failure alerts (a crash-respawning
    /// remote pane must not storm the inbox) — see `record`.
    debounce: Mutex<thegn_core::notify_debounce::NotifyDebounce>,
    /// Manual DND override: `None` defers to the schedule.
    dnd_forced: Mutex<Option<bool>>,
    /// Active routing mode (`""` = the default/no-mode rule set).
    active_mode: Mutex<String>,
    /// Active profile name (`""` = none); matched by a rule's `profile`.
    active_profile: String,
    /// Set when a terminal `BEL` should be written on the next render flush.
    pending_bell: AtomicBool,
    /// Wakes the event loop so a latched bell (or DND/mode chip change) paints.
    waker: TerminalWaker,
}

impl NotifyState {
    /// Build a handle from the effective notification config. `active_profile`
    /// is the resolved profile name (may be empty).
    pub fn new(
        cfg: NotificationsConfig,
        active_profile: String,
        waker: TerminalWaker,
    ) -> std::sync::Arc<Self> {
        let active_mode = cfg.active_mode.clone();
        std::sync::Arc::new(NotifyState {
            cfg: Mutex::new(cfg),
            debounce: Mutex::new(thegn_core::notify_debounce::NotifyDebounce::default()),
            dnd_forced: Mutex::new(None),
            active_mode: Mutex::new(active_mode),
            active_profile,
            pending_bell: AtomicBool::new(false),
            waker,
        })
    }

    /// Replace the effective config after a live reload.
    pub fn update_cfg(&self, cfg: NotificationsConfig) {
        // Keep the runtime mode if it is still a valid mode (or empty); else
        // reset to the new config's default.
        {
            let mut mode = self.active_mode.lock().unwrap();
            if !mode.is_empty() && !cfg.modes.contains_key(&*mode) {
                *mode = cfg.active_mode.clone();
            }
        }
        *self.cfg.lock().unwrap() = cfg;
    }

    fn route_ctx(&self) -> RouteCtx {
        RouteCtx {
            now_local: Some(chrono::Local::now().naive_local()),
            dnd_forced: *self.dnd_forced.lock().unwrap(),
            active_mode: self.active_mode.lock().unwrap().clone(),
            active_profile: self.active_profile.clone(),
        }
    }

    /// Decide how to route a notification of the given (snake_case) kind. An
    /// unknown kind routes permissively (record + desktop, no sound) so novel
    /// kinds are never silently swallowed.
    pub fn decide(
        &self,
        kind: &str,
        source_ref: &str,
        message: &str,
        worktree: &str,
    ) -> RouteDecision {
        let Some(kind) = parse_kind(kind) else {
            return RouteDecision {
                record: true,
                effective_priority: thegn_core::notification::Priority::Notice,
                desktop: true,
                toast: false,
                sound: None,
            };
        };
        let cfg = self.cfg.lock().unwrap();
        decide(kind, source_ref, message, worktree, &cfg, &self.route_ctx())
    }

    /// Ring the resolved sound: latch the terminal bell (painted by the render
    /// loop) or spawn the configured command off-thread. Best-effort.
    pub fn emit_sound(&self, decision: &RouteDecision) {
        match &decision.sound {
            Some(SoundEmit::Bell) => self.ring_bell(),
            Some(SoundEmit::Command(cmd)) => spawn_sound_command(cmd),
            None => {}
        }
    }

    /// Latch a terminal bell and wake the loop to flush it.
    pub fn ring_bell(&self) {
        self.pending_bell.store(true, Ordering::Relaxed);
        let _ = self.waker.wake();
    }

    /// Consume the latched bell (called once per render flush by the loop).
    pub fn take_bell(&self) -> bool {
        self.pending_bell.swap(false, Ordering::Relaxed)
    }

    /// Toggle the manual DND override; returns the new resolved DND state.
    pub fn toggle_dnd(&self) -> bool {
        let now = self.dnd_active();
        *self.dnd_forced.lock().unwrap() = Some(!now);
        let _ = self.waker.wake();
        !now
    }

    /// The currently resolved DND state (manual override, else the schedule).
    pub fn dnd_active(&self) -> bool {
        if let Some(forced) = *self.dnd_forced.lock().unwrap() {
            return forced;
        }
        let cfg = self.cfg.lock().unwrap();
        thegn_core::notification_route::scheduled_dnd_active(
            &cfg.dnd,
            Some(chrono::Local::now().naive_local()),
        )
    }

    /// Advance the active routing mode to the next configured mode (wrapping
    /// through the empty "no mode" state). Returns the new mode name.
    pub fn cycle_mode(&self) -> String {
        let cfg = self.cfg.lock().unwrap();
        let mut names: Vec<String> = std::iter::once(String::new())
            .chain(cfg.modes.keys().cloned())
            .collect();
        names.dedup();
        drop(cfg);
        let mut mode = self.active_mode.lock().unwrap();
        let idx = names.iter().position(|m| m == &*mode).unwrap_or(0);
        let next = names[(idx + 1) % names.len()].clone();
        *mode = next.clone();
        let _ = self.waker.wake();
        next
    }

    /// The active routing mode (`""` = none).
    pub fn active_mode(&self) -> String {
        self.active_mode.lock().unwrap().clone()
    }
}

/// Decide + conditionally persist a notification. Returns the decision and the
/// new inbox row id (`None` when a rule dropped it). The dispatch sites use the
/// returned decision to gate the desktop toast + sound.
pub fn record(
    db: &thegn_core::db::Db,
    state: &NotifyState,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> (RouteDecision, Option<i64>) {
    // Burst suppression for repeat failure alerts: a crash-respawning pane on
    // a flaky remote fires an identical process_failed every few seconds; one
    // per window is signal, the rest are inbox noise.
    if kind == "process_failed" {
        let now = chrono::Local::now().timestamp();
        if !state.debounce.lock().unwrap().allow(worktree, kind, now) {
            return (
                RouteDecision {
                    record: false,
                    effective_priority: thegn_core::notification::Priority::Notice,
                    desktop: false,
                    toast: false,
                    sound: None,
                },
                None,
            );
        }
    }
    let decision = state.decide(kind, source_ref, message, worktree);
    let id = if decision.record {
        db.put_notification(kind, source_ref, message, worktree)
            .ok()
    } else {
        None
    };
    (decision, id)
}

fn parse_kind(s: &str) -> Option<NotificationKind> {
    NotificationKind::ALL.into_iter().find(|k| k.as_str() == s)
}

/// Run a sound command line off-thread via `sh -c`, fully detached. Best-effort:
/// a missing shell or a failing command is swallowed — a sound must never
/// disrupt the session.
// off-loop: the wait happens on the detached "notify-sound" std::thread below.
#[expect(clippy::disallowed_methods)]
fn spawn_sound_command(cmd: &str) {
    let cmd = cmd.to_string();
    std::thread::Builder::new()
        .name("notify-sound".into())
        .spawn(move || {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_roundtrip() {
        assert_eq!(
            parse_kind("test_failed"),
            Some(NotificationKind::TestFailed)
        );
        assert_eq!(parse_kind("bogus"), None);
    }
}
