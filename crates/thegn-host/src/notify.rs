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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::{NotificationsConfig, PushKind};
use thegn_core::notification::NotificationKind;
use thegn_core::notification_render::{MarkdownFlavor, render};
use thegn_core::notification_route::{RouteCtx, RouteDecision, SoundEmit, decide};

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
    /// Off-loop provider/pack runtime. Its snapshot is swapped after config
    /// reload, while producers only perform a bounded queue send.
    sound_runtime: Arc<crate::notification_sound::SoundRuntime>,
    /// Sender for the transient in-app toast projection, installed once at loop
    /// startup ([`Self::set_toast_tx`]). `None` before wiring (or in headless tests),
    /// so an emit is a silent no-op rather than a panic.
    toast_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>>>,
    /// Bounded sender to the push-to-phone publisher worker, installed at
    /// startup ([`Self::set_push_tx`]) only when `[notifications.push]` is
    /// configured. `None` ⇒ push is unconfigured and every emit is a silent
    /// no-op. Bounded so a stalled server can never grow this without limit;
    /// overflow drops (best-effort delivery — the inbox row is the durable
    /// record) and increments [`Self::push_dropped`].
    push_tx: Mutex<Option<std::sync::mpsc::SyncSender<crate::push_notify::PushJob>>>,
    /// Count of push jobs dropped because the worker's queue was full.
    push_dropped: std::sync::atomic::AtomicU64,
    /// Per-sink delivery counters shared with the loop-owned Monitor model.
    delivery: crate::notification_delivery::DeliverySnapshot,
    /// Wakes the event loop so a latched bell (or DND/mode chip change) paints.
    waker: TerminalWaker,
}

fn global_slot() -> &'static Mutex<Weak<NotifyState>> {
    static SLOT: OnceLock<Mutex<Weak<NotifyState>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Weak::new()))
}

/// The hydration worker has no loop-owned argument, but still belongs to the
/// single UI notification route. This weak handle avoids keeping a compositor
/// alive after shutdown.
pub(crate) fn global() -> Option<Arc<NotifyState>> {
    global_slot().lock().unwrap().upgrade()
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
        let sound_cfg = cfg.sound.clone();
        let sound_runtime = crate::notification_sound::SoundRuntime::new(waker.clone());
        let state = Arc::new(NotifyState {
            cfg: Mutex::new(cfg),
            debounce: Mutex::new(thegn_core::notify_debounce::NotifyDebounce::default()),
            dnd_forced: Mutex::new(None),
            active_mode: Mutex::new(active_mode),
            active_profile,
            pending_bell: AtomicBool::new(false),
            focused_worktree: Mutex::new(String::new()),
            sound_runtime: Arc::clone(&sound_runtime),
            toast_tx: Mutex::new(None),
            push_tx: Mutex::new(None),
            push_dropped: std::sync::atomic::AtomicU64::new(0),
            delivery: crate::notification_delivery::DeliverySnapshot::with_waker(waker.clone()),
            waker,
        });
        *global_slot().lock().unwrap() = Arc::downgrade(&state);
        sound_runtime.reload(sound_cfg);
        state
    }

    pub(crate) fn delivery_snapshot(&self) -> crate::notification_delivery::DeliverySnapshot {
        self.delivery.clone()
    }

    /// Install the bounded sender to the push publisher worker (wired at startup
    /// only when `[notifications.push]` is configured).
    pub fn set_push_tx(&self, tx: std::sync::mpsc::SyncSender<crate::push_notify::PushJob>) {
        *self.push_tx.lock().unwrap() = Some(tx);
    }

    /// Stop routing to the previous worker before a config reload installs a
    /// replacement. Dropping the sender closes the old bounded queue once the
    /// worker drains its current job.
    pub fn clear_push_tx(&self) {
        *self.push_tx.lock().unwrap() = None;
    }

    /// Hand a routed notification to the push publisher worker, iff the routing
    /// decision authorised the `push` channel. Off-loop and non-blocking: a
    /// bounded `try_send` that drops (with a counter) rather than block when the
    /// worker is backed up behind a slow server. A silent no-op when push is
    /// unconfigured (no worker installed).
    pub fn emit_push(
        &self,
        decision: &RouteDecision,
        kind: &str,
        source_ref: &str,
        title: &str,
        body: &str,
        worktree: &str,
    ) {
        if decision.push_sinks.is_empty() {
            return;
        }
        let guard = self.push_tx.lock().unwrap();
        let Some(tx) = guard.as_ref() else {
            return; // push unconfigured
        };
        let Some(notification_kind) = parse_kind(kind) else {
            return;
        };
        let content = if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n{body}")
        };
        let sink_kinds: std::collections::BTreeMap<String, PushKind> = self
            .cfg
            .lock()
            .unwrap()
            .push
            .effective_sinks()
            .into_iter()
            .map(|sink| (sink.name, sink.kind))
            .collect();
        for sink in &decision.push_sinks {
            let flavor = sink_kinds
                .get(sink)
                .copied()
                .map(push_flavor)
                .unwrap_or(MarkdownFlavor::Plain);
            let job = crate::push_notify::PushJob {
                sink: sink.clone(),
                title: title.to_string(),
                body: body.to_string(),
                priority: decision.effective_priority,
                kind: kind.to_string(),
                worktree: worktree.to_string(),
                rendered: Some(render(
                    notification_kind,
                    decision.effective_priority,
                    &content,
                    source_ref,
                    worktree,
                    thegn_core::util::now(),
                    flavor,
                )),
            };
            match tx.try_send(job) {
                Ok(()) => self
                    .delivery
                    .event(sink, crate::notification_delivery::DeliveryEvent::Queued),
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    self.delivery
                        .event(sink, crate::notification_delivery::DeliveryEvent::QueueDrop);
                    // best-effort delivery: the inbox row is the durable record. Surface
                    // the running drop total so a stalled server is visible in the log.
                    let total = self.push_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::warn!(target: "thegn::push", sink = %sink, dropped_total = total, "push queue full — dropped a notification");
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
            }
        }
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
            let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    }

    /// Update the focused/visible worktree path (drives `suppress_focused`).
    pub fn set_focused_worktree(&self, worktree: String) {
        *self.focused_worktree.lock().unwrap() = worktree;
    }

    /// Replace the effective config after a live reload.
    pub fn update_cfg(&self, cfg: NotificationsConfig) {
        self.sound_runtime.reload(cfg.sound.clone());
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
                // Unknown kinds don't push (conservative — a novel kind reaches
                // the inbox + desktop, but not a phone, until it's modelled).
                push_sinks: Vec::new(),
                sound: None,
            };
        };
        let cfg = self.cfg.lock().unwrap();
        decide(kind, source_ref, message, worktree, &cfg, &self.route_ctx())
    }

    /// Queue the resolved sound or latch the terminal bell. Best-effort and
    /// non-blocking for every producer.
    pub fn emit_sound(&self, decision: &RouteDecision) {
        match &decision.sound {
            Some(sound @ (SoundEmit::File { .. } | SoundEmit::Command(_))) => {
                crate::notification_sound::emit(&self.sound_runtime, sound);
            }
            Some(SoundEmit::Bell) => self.ring_bell(),
            None => {}
        }
    }

    /// Latch a terminal bell and wake the loop to flush it.
    pub fn ring_bell(&self) {
        self.pending_bell.store(true, Ordering::Relaxed);
        let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    }

    /// Consume the latched bell (called once per render flush by the loop).
    pub fn take_bell(&self) -> bool {
        // Read both latches before combining them. Short-circuiting here would
        // leave a fallback latch set whenever a normal BEL was also pending,
        // replaying that fallback on the following frame.
        let pending = self.pending_bell.swap(false, Ordering::Relaxed);
        let fallback = self.sound_runtime.take_fallback_bell();
        pending || fallback
    }

    /// Toggle the manual DND override; returns the new resolved DND state.
    pub fn toggle_dnd(&self) -> bool {
        let now = self.dnd_active();
        *self.dnd_forced.lock().unwrap() = Some(!now);
        let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
        let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        next
    }

    /// The active routing mode (`""` = none).
    pub fn active_mode(&self) -> String {
        self.active_mode.lock().unwrap().clone()
    }
}

/// Decide and emit all transient channels for producers that have no DB handle.
/// DB-backed producers use [`record`], which records before calling the same
/// emission steps.
pub(crate) fn route(
    state: &NotifyState,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> RouteDecision {
    let decision = state.decide(kind, source_ref, message, worktree);
    state.emit_sound(&decision);
    if decision.toast {
        state.emit_toast(message, decision.effective_priority);
    }
    state.emit_push(&decision, kind, source_ref, message, "", worktree);
    decision
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
    record_with_facts(
        db,
        state,
        kind,
        source_ref,
        message,
        worktree,
        crate::automation_events::EventFacts::default(),
    )
}

pub(crate) fn record_with_facts(
    db: &thegn_core::db::Db,
    state: &NotifyState,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    facts: crate::automation_events::EventFacts,
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
                    push_sinks: Vec::new(),
                    sound: None,
                },
                None,
            );
        }
    }
    let decision = state.decide(kind, source_ref, message, worktree);
    let id = crate::automation_events::insert_routed(
        db, kind, source_ref, message, worktree, facts, &decision, false,
    )
    .unwrap_or_else(|error| {
        tracing::debug!(target: "thegn::notify", %error, "notification cache write failed");
        None
    });
    emit_channels(state, &decision, kind, source_ref, message, worktree);
    (decision, id)
}

pub(crate) fn record_once_with_facts(
    db: &thegn_core::db::Db,
    state: &NotifyState,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    facts: crate::automation_events::EventFacts,
) -> (RouteDecision, bool) {
    let decision = state.decide(kind, source_ref, message, worktree);
    let inserted = crate::automation_events::insert_routed(
        db, kind, source_ref, message, worktree, facts, &decision, true,
    )
    .map(|id| id.is_some())
    .unwrap_or_else(|error| {
        tracing::debug!(target: "thegn::notify", %error, "emit-once notification cache write failed");
        false
    });
    if inserted {
        emit_channels(state, &decision, kind, source_ref, message, worktree);
    }
    (decision, inserted)
}

#[cfg(test)]
fn record_once(
    db: &thegn_core::db::Db,
    state: &NotifyState,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> (RouteDecision, bool) {
    record_once_with_facts(
        db,
        state,
        kind,
        source_ref,
        message,
        worktree,
        crate::automation_events::EventFacts::default(),
    )
}

fn emit_channels(
    state: &NotifyState,
    decision: &RouteDecision,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) {
    state.emit_sound(decision);
    // The transient in-app toast is the one funnel for routed events: it fires
    // iff the routing decision authorizes it (`toast`), governed by the same
    // rules/DND as every other channel — never a hand-rolled toast that dodges
    // routing.
    if decision.toast {
        state.emit_toast(message, decision.effective_priority);
    }
    // Push-to-phone rides the same decision. The publisher worker exists only
    // when `[notifications.push]` is configured; otherwise this is a no-op.
    state.emit_push(decision, kind, source_ref, message, "", worktree);
}

fn parse_kind(s: &str) -> Option<NotificationKind> {
    NotificationKind::ALL.into_iter().find(|k| k.as_str() == s)
}

fn push_flavor(kind: PushKind) -> MarkdownFlavor {
    match kind {
        PushKind::Webhook => MarkdownFlavor::CommonMark,
        PushKind::Discord => MarkdownFlavor::Discord,
        PushKind::Slack => MarkdownFlavor::Slack,
        PushKind::Ntfy | PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => {
            MarkdownFlavor::Plain
        }
    }
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

    #[cfg(unix)]
    fn test_state(
        cfg: NotificationsConfig,
    ) -> (
        Arc<NotifyState>,
        std::fs::File,
        termwiz::terminal::UnixTerminal,
    ) {
        use std::os::fd::FromRawFd;
        use termwiz::terminal::{Terminal, UnixTerminal};

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let caps =
            termwiz::caps::Capabilities::new_with_hints(termwiz::caps::ProbeHints::default())
                .unwrap();
        let terminal = UnixTerminal::new_with(caps, &slave, &slave).unwrap();
        let state = NotifyState::new(cfg, String::new(), terminal.waker());
        (state, master, terminal)
    }

    #[cfg(unix)]
    #[test]
    fn hydration_emit_once_sounds_mentions_and_overdue_only_on_insert() {
        use std::collections::BTreeMap;
        use thegn_core::config::{NotificationRule, NotificationsConfig};
        use thegn_core::notification_route::SoundEmit;

        let mut cfg = NotificationsConfig::default();
        cfg.sound.min_priority = "info".into();
        cfg.sound.per_kind = BTreeMap::from([
            ("mentioned".into(), "bell".into()),
            ("overdue".into(), "bell".into()),
        ]);
        let (state, _master, _terminal) = test_state(cfg.clone());
        let db = thegn_core::db::Db::open_memory().unwrap();

        for (kind, source_ref, message) in [
            ("mentioned", "ghn:42:1", "mentioned in issue: fix it"),
            ("overdue", "linear:A-1", "A-1 overdue (was due 2026-08-01)"),
        ] {
            let (decision, inserted) =
                record_once(&db, &state, kind, source_ref, message, "/wt/app");
            assert!(inserted, "first {kind} observation must insert");
            assert_eq!(decision.sound, Some(SoundEmit::Bell));
            assert!(state.take_bell(), "first {kind} observation must sound");

            let (decision, inserted) =
                record_once(&db, &state, kind, source_ref, message, "/wt/app");
            assert!(!inserted, "second {kind} observation must dedupe");
            assert_eq!(decision.sound, Some(SoundEmit::Bell));
            assert!(!state.take_bell(), "duplicate {kind} must not sound");
        }

        let mut drop_cfg = cfg.clone();
        drop_cfg.rules.push(NotificationRule {
            kind: Some("mentioned".into()),
            drop: true,
            ..Default::default()
        });
        let (drop_state, _master, _terminal) = test_state(drop_cfg);
        let db = thegn_core::db::Db::open_memory().unwrap();
        let (decision, inserted) = record_once(
            &db,
            &drop_state,
            "mentioned",
            "ghn:drop",
            "dropped",
            "/wt/app",
        );
        assert!(!inserted);
        assert!(!decision.record);
        assert_eq!(decision.sound, None);
        assert!(!drop_state.take_bell());

        let (dnd_state, _master, _terminal) = test_state(cfg.clone());
        assert!(dnd_state.toggle_dnd());
        let db = thegn_core::db::Db::open_memory().unwrap();
        let (decision, inserted) =
            record_once(&db, &dnd_state, "overdue", "linear:dnd", "dnd", "/wt/app");
        assert!(inserted);
        assert_eq!(decision.sound, None);
        assert!(!dnd_state.take_bell());

        let (focused_state, _master, _terminal) = test_state(cfg);
        focused_state.set_focused_worktree("/wt/app".into());
        let db = thegn_core::db::Db::open_memory().unwrap();
        let (decision, inserted) = record_once(
            &db,
            &focused_state,
            "mentioned",
            "ghn:focused",
            "focused",
            "/wt/app",
        );
        assert!(inserted);
        assert_eq!(decision.sound, None);
        assert!(!focused_state.take_bell());
    }

    #[cfg(unix)]
    #[test]
    fn take_bell_consumes_normal_and_fallback_latches_together() {
        let (state, _master, _terminal) = test_state(NotificationsConfig::default());
        state.ring_bell();
        state.sound_runtime.latch_fallback_bell_for_test();

        assert!(state.take_bell());
        assert!(!state.take_bell());
    }
}
