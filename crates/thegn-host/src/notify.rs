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
    /// Currently-focused/visible worktree path (`""` = none). Fed into the
    /// routing context so `[notifications.sound] suppress_focused` can silence a
    /// cue for the worktree the user is already looking at.
    focused_worktree: Mutex<String>,
    /// The configured chime file (`[notifications.sound] chime_file`), empty ⇒
    /// the bundled chime. Read on every `Chime` emit.
    chime_file: Mutex<String>,
    /// Sender for the transient in-app toast projection, installed once at loop
    /// startup ([`Self::set_toast_tx`]). `None` before wiring (or in headless tests),
    /// so an emit is a silent no-op rather than a panic.
    toast_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>>>,
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
        let chime_file = cfg.sound.chime_file.clone();
        std::sync::Arc::new(NotifyState {
            cfg: Mutex::new(cfg),
            debounce: Mutex::new(thegn_core::notify_debounce::NotifyDebounce::default()),
            dnd_forced: Mutex::new(None),
            active_mode: Mutex::new(active_mode),
            active_profile,
            pending_bell: AtomicBool::new(false),
            focused_worktree: Mutex::new(String::new()),
            chime_file: Mutex::new(chime_file),
            toast_tx: Mutex::new(None),
            waker,
        })
    }

    /// Install the loop's refresh channel so routed notifications can project a
    /// transient in-app toast. Called once at startup, after the loop owns
    /// `refresh_tx`.
    pub fn set_toast_tx(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    ) {
        *self.toast_tx.lock().unwrap() = Some(tx);
    }

    /// Project a routed notification onto the in-app toast stack — the transient
    /// twin of the persistent inbox entry. Best-effort (a gone loop / unwired
    /// channel just drops it) and cheap (a channel send + waker pulse), so it is
    /// safe on any dispatch thread.
    pub fn emit_toast(&self, message: &str, priority: thegn_core::notification::Priority) {
        if let Some(tx) = self.toast_tx.lock().unwrap().as_ref()
            && tx
                .send(crate::hydrate::RefreshKind::Toast {
                    message: message.to_string(),
                    priority,
                })
                .is_ok()
        {
            let _ = self.waker.wake();
        }
    }

    /// Update the focused/visible worktree path (drives `suppress_focused`).
    pub fn set_focused_worktree(&self, worktree: String) {
        *self.focused_worktree.lock().unwrap() = worktree;
    }

    /// Replace the effective config after a live reload.
    pub fn update_cfg(&self, cfg: NotificationsConfig) {
        *self.chime_file.lock().unwrap() = cfg.sound.chime_file.clone();
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
            focused_worktree: self.focused_worktree.lock().unwrap().clone(),
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
            Some(SoundEmit::Chime) => {
                let file = self.chime_file.lock().unwrap().clone();
                // No system player/file ⇒ fall back to the terminal bell so a
                // chime is never a silent no-op.
                if !crate::chime::play(&file) {
                    self.ring_bell();
                }
            }
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
    // The transient in-app toast is the one funnel for routed events: it fires
    // iff the routing decision authorizes it (`toast`), governed by the same
    // rules/DND as every other channel — never a hand-rolled toast that dodges
    // routing.
    if decision.toast {
        state.emit_toast(message, decision.effective_priority);
    }
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
pub(crate) fn spawn_sound_command(cmd: &str) {
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
