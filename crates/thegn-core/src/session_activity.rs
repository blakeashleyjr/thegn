//! Per-**session** activity: the pane daemon's observer of the same FSM the
//! compositor's per-**worktree** dots run on.
//!
//! [`crate::activity`] answers "does this sidebar row need a dot": it is keyed by
//! worktree path, walks `/proc`, and persists to a file. That is the right shape
//! for a compositor that renders rows — and the wrong shape for a daemon that
//! owns PTYs. A supervisor asking "is *this agent* still working?" needs an
//! answer keyed by session, available with no UI running, and derived from the
//! bytes the daemon already has in hand.
//!
//! So there are two observers and **one decision function**. Both fold their
//! observations through [`crate::activity_step::step`], both judge output
//! freshness with [`crate::activity_step::output_is_fresh`], and both apply the
//! same unsolicited-output rule ([`unsolicited_age`], which lives here and which
//! the compositor's publisher calls too). They cannot drift into disagreeing
//! about what "quiet" means, because there is nothing to drift.
//!
//! What this observer does **not** have is CPU. The compositor sums jiffies over
//! every process under a worktree; a session knows only its own PTY. Summing CPU
//! per worktree here would let a sibling `cargo build` pin an agent "working"
//! and silently break `wait(idle)`, so [`Observation::cpu_busy`] exists as a
//! seam and nothing sets it today. In practice a working agent redraws — that is
//! the whole premise of the output signal (see `crate::activity`'s docs).
//!
//! # The observation window
//!
//! `output_is_fresh` takes a `wall` window because the compositor polls on a
//! cadence and must catch output that landed anywhere inside it. This observer
//! has no cadence — it is woken by the bytes themselves — so it passes
//! `wall = 0.0` and the window collapses to the `output_hint_ttl` floor. That
//! makes the busy horizon a **constant** (`output_hint_ttl` + the future slack),
//! which is what lets [`SessionActivity::next_tick`] name an exact deadline
//! instead of heartbeating: after the last byte, a session wakes exactly twice
//! (once to notice the output went stale, once when the quiet grace elapses) and
//! then arms no timer at all.

use crate::activity_step::{self, Agentness, Marks, Signals};
use crate::attention::ActivityKind;
use crate::config_activity::ActivityConfig;

/// Never schedule a wake closer than this: a deadline that has already passed
/// must still yield a real sleep rather than a spin.
const MIN_TICK_SECS: f64 = 0.05;

/// Mirrors `activity_step::FUTURE_SLACK_SECS`, which is private. Used only to
/// compute the *deadline* at which a stamp stops being fresh; freshness itself
/// is always decided by `output_is_fresh`, never by this constant.
const FUTURE_SLACK_SECS: f64 = 1.0;

/// The age of the last **unsolicited** output, or `None` when there is no
/// output, the output is echo (input within the configured gap before it), or
/// the pane/session is still inside the spawn grace.
///
/// Lifted out of the host's output-hint publisher so both observers share one
/// rule — and so the `[activity] spawn_grace_secs` / `unsolicited_gap_secs`
/// knobs, which the config reference has always documented as configurable,
/// actually govern it. Pure: ages in seconds, clock injected by the caller.
pub fn unsolicited_age(
    out_age: Option<f64>,
    in_age: Option<f64>,
    age_since_start: Option<f64>,
    cfg: &ActivityConfig,
) -> Option<f64> {
    let out = out_age?;
    if age_since_start.is_none_or(|a| a < cfg.spawn_grace()) {
        return None;
    }
    // Timestamp form "output later than input + gap", in age space: the input
    // must be older than the output by more than the gap.
    if in_age.is_some_and(|inp| inp - out <= cfg.unsolicited_gap()) {
        return None;
    }
    Some(out)
}

/// One observation folded into a session's marks.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    /// Unix seconds.
    pub now: f64,
    /// Whether a real agent is bound to (or observed in) this session.
    pub agent: Agentness,
    /// Reserved for a future CPU observer. Always `false` in the daemon today —
    /// see the module docs for why a per-session CPU signal is not free.
    pub cpu_busy: bool,
}

/// One session's activity: the marks the FSM carries between observations, plus
/// the stamps that produce the busy signal.
#[derive(Debug, Clone)]
pub struct SessionActivity {
    marks: Marks,
    started_at: f64,
    last_output_at: Option<f64>,
    /// Input written toward the child, or a repaint we asked for. Both suppress
    /// the output that follows — see [`SessionActivity::note_solicited`].
    last_input_at: Option<f64>,
    /// Whether this session has ever been observed busy. `wait(idle)` reads it
    /// so "wait until the agent finishes" is never answered instantly by a
    /// session that has not started yet.
    ever_busy: bool,
}

impl SessionActivity {
    /// A fresh session, born at `started_at` (unix seconds).
    pub fn new(started_at: f64) -> Self {
        Self {
            marks: Marks {
                state: crate::activity::SETTLED_STATE.to_string(),
                quiet_since: None,
                busy_since: None,
                last_active_at: None,
            },
            started_at,
            last_output_at: None,
            last_input_at: None,
            ever_busy: false,
        }
    }

    /// The PTY produced bytes at `at`.
    pub fn note_output(&mut self, at: f64) {
        self.last_output_at = Some(at);
    }

    /// Bytes were written toward the child at `at` — the echo suppressor.
    pub fn note_input(&mut self, at: f64) {
        self.last_input_at = Some(at);
    }

    /// A repaint *we* caused (a resize's SIGWINCH, a reattach replaying
    /// scrollback) is about to arrive: mark it solicited so the redraw does not
    /// read as agent work.
    ///
    /// Identical to [`Self::note_input`] by construction — the compositor's
    /// `PtyPane::mark_output_solicited` stamps the input clock for exactly this
    /// reason. Kept as its own name because the *call sites* mean different
    /// things, and a reader of `on_resize` should not have to know that a
    /// resize counts as "input".
    pub fn note_solicited(&mut self, at: f64) {
        self.note_input(at);
    }

    /// Fold one observation and return the resulting kind.
    pub fn observe(&mut self, obs: Observation, cfg: &ActivityConfig) -> ActivityKind {
        let busy = obs.cpu_busy || self.output_busy(obs.now, cfg);
        if busy {
            self.ever_busy = true;
        }
        let marks = std::mem::replace(&mut self.marks, placeholder_marks());
        self.marks = activity_step::step(
            marks,
            Signals {
                busy,
                agent: obs.agent,
            },
            cfg,
            obs.now,
        );
        self.kind()
    }

    /// Whether fresh unsolicited output makes this session busy at `now`.
    ///
    /// `wall = 0.0` on purpose: this observer has no poll interval, so the
    /// window is the `output_hint_ttl` floor alone (module docs).
    fn output_busy(&self, now: f64, cfg: &ActivityConfig) -> bool {
        let Some(stamp) = self.last_output_at else {
            return false;
        };
        let unsolicited = unsolicited_age(
            Some(now - stamp),
            self.last_input_at.map(|i| now - i),
            Some(now - self.started_at),
            cfg,
        );
        unsolicited.is_some() && activity_step::output_is_fresh(stamp, now, 0.0, cfg)
    }

    /// The FSM state as the attention model's vocabulary.
    pub fn kind(&self) -> ActivityKind {
        match self.marks.state.as_str() {
            "active" => ActivityKind::Active,
            "waiting" => ActivityKind::Waiting,
            "read" => ActivityKind::Read,
            _ => ActivityKind::None,
        }
    }

    /// The marks, for persistence or inspection.
    pub fn marks(&self) -> &Marks {
        &self.marks
    }

    /// Whether this session has ever been observed busy.
    pub fn ever_busy(&self) -> bool {
        self.ever_busy
    }

    /// When the current state was entered, as the FSM knows it: the quiet-streak
    /// start for a settled/waiting session, else the last busy observation.
    pub fn state_since(&self) -> Option<f64> {
        self.marks.quiet_since.or(self.marks.last_active_at)
    }

    /// A human looked: `waiting` → `read`, the same ack edge
    /// [`crate::activity::ack`] applies when a tab is focused.
    pub fn mark_read(&mut self) {
        if self.marks.state == "waiting" {
            self.marks.state = "read".to_string();
        }
    }

    /// Seconds until the next observation is worth taking, or `None` when the
    /// session is settled and **no timer should be armed**.
    ///
    /// This is the whole idle contract: an agent that has finished costs nothing
    /// until its next byte. Three things can still move the state — a fresh
    /// output stamp that will go stale, an open quiet streak that will elapse
    /// its grace, and (under a red dot) a busy streak that will elapse its
    /// resume grace — so the deadline is the earliest of whichever apply.
    pub fn next_tick(&self, cfg: &ActivityConfig, now: f64) -> Option<f64> {
        let mut deadline: Option<f64> = None;
        let mut consider = |at: f64| {
            deadline = Some(deadline.map_or(at, |d: f64| d.min(at)));
        };

        // Output that is fresh now stops being fresh at a knowable instant.
        if let Some(stamp) = self.last_output_at
            && self.output_busy(now, cfg)
        {
            consider(stamp + cfg.output_hint_ttl() + FUTURE_SLACK_SECS);
        }
        // A quiet streak under `active` will arm the finished state.
        if self.marks.state == "active"
            && let Some(quiet) = self.marks.quiet_since
        {
            consider(quiet + cfg.quiet_grace());
        }
        // A busy streak under a red dot will clear it once sustained.
        if matches!(self.marks.state.as_str(), "waiting" | "read")
            && let Some(busy) = self.marks.busy_since
        {
            consider(busy + cfg.resume_grace());
        }

        deadline.map(|at| (at - now).max(MIN_TICK_SECS))
    }
}

/// A cheap stand-in so `observe` can move the real marks through `step` without
/// cloning a `String` on every observation.
fn placeholder_marks() -> Marks {
    Marks {
        state: String::new(),
        quiet_since: None,
        busy_since: None,
        last_active_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ActivityConfig {
        ActivityConfig::default()
    }

    fn obs(now: f64) -> Observation {
        Observation {
            now,
            agent: Agentness::Present,
            cpu_busy: false,
        }
    }

    /// Past the spawn grace, so output actually counts.
    const T0: f64 = 1000.0;
    const START: f64 = 900.0;

    fn started() -> SessionActivity {
        SessionActivity::new(START)
    }

    #[test]
    fn output_makes_a_session_working() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        assert_eq!(a.observe(obs(T0), &c), ActivityKind::Active);
        assert!(a.ever_busy());
    }

    #[test]
    fn a_fresh_session_is_settled_and_arms_no_timer() {
        let c = cfg();
        let a = started();
        assert_eq!(a.kind(), ActivityKind::None);
        assert_eq!(a.next_tick(&c, T0), None, "nothing to wait for");
        assert!(!a.ever_busy());
    }

    /// The core latency contract: after the last byte a session wakes exactly
    /// twice — once when the output goes stale, once when the quiet grace
    /// elapses — and then arms nothing.
    #[test]
    fn settling_takes_exactly_two_wakeups_then_no_timer() {
        let c = cfg(); // ttl 6.0, slack 1.0, quiet grace 8.0
        let mut a = started();
        a.note_output(T0);
        assert_eq!(a.observe(obs(T0), &c), ActivityKind::Active);

        // First wake: exactly when the stamp stops being fresh.
        let d1 = a.next_tick(&c, T0).expect("a busy session must tick");
        assert!((d1 - 7.0).abs() < 1e-9, "stale at ttl + slack, got {d1}");
        let t1 = T0 + d1;
        assert_eq!(
            a.observe(obs(t1), &c),
            ActivityKind::Active,
            "the quiet streak only opens here"
        );

        // Second wake: exactly the quiet grace later.
        let d2 = a.next_tick(&c, t1).expect("a quiet streak must tick");
        assert!((d2 - 8.0).abs() < 1e-9, "quiet grace, got {d2}");
        let t2 = t1 + d2;
        assert_eq!(a.observe(obs(t2), &c), ActivityKind::Waiting, "finished");

        // And now nothing.
        assert_eq!(
            a.next_tick(&c, t2),
            None,
            "a settled session must arm no timer"
        );
    }

    #[test]
    fn keystroke_echo_is_not_work() {
        let c = cfg();
        let mut a = started();
        a.note_input(T0);
        a.note_output(T0 + 0.2); // echo, well inside the 1.0s gap
        assert_eq!(a.observe(obs(T0 + 0.2), &c), ActivityKind::None);
        assert!(!a.ever_busy());
    }

    #[test]
    fn output_well_after_input_is_work() {
        let c = cfg();
        let mut a = started();
        a.note_input(T0);
        a.note_output(T0 + 3.0); // past the gap
        assert_eq!(a.observe(obs(T0 + 3.0), &c), ActivityKind::Active);
    }

    #[test]
    fn a_solicited_repaint_is_not_work() {
        let c = cfg();
        let mut a = started();
        a.note_solicited(T0); // a resize / reattach replay
        a.note_output(T0 + 0.1);
        assert_eq!(a.observe(obs(T0 + 0.1), &c), ActivityKind::None);
    }

    #[test]
    fn spawn_grace_suppresses_startup_noise() {
        let c = cfg(); // spawn grace 5.0
        let mut a = SessionActivity::new(T0);
        a.note_output(T0 + 1.0); // banner, prompt paint
        assert_eq!(a.observe(obs(T0 + 1.0), &c), ActivityKind::None);
        // Past the grace, the same shape of output counts.
        a.note_output(T0 + 9.0);
        assert_eq!(a.observe(obs(T0 + 9.0), &c), ActivityKind::Active);
    }

    #[test]
    fn a_busy_blip_restarts_the_quiet_streak() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        a.observe(obs(T0), &c);
        // Go stale, opening the streak.
        a.observe(obs(T0 + 7.0), &c);
        assert!(a.marks().quiet_since.is_some());
        // The agent speaks again.
        a.note_output(T0 + 9.0);
        a.observe(obs(T0 + 9.0), &c);
        assert_eq!(a.marks().quiet_since, None, "the streak is cleared");
        assert_eq!(a.kind(), ActivityKind::Active);
    }

    #[test]
    fn mark_read_acks_a_finished_agent() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        a.observe(obs(T0), &c);
        a.observe(obs(T0 + 7.0), &c);
        a.observe(obs(T0 + 16.0), &c);
        assert_eq!(a.kind(), ActivityKind::Waiting);
        a.mark_read();
        assert_eq!(a.kind(), ActivityKind::Read);
        // Idempotent, and never drags a working session backwards.
        a.mark_read();
        assert_eq!(a.kind(), ActivityKind::Read);
    }

    #[test]
    fn a_red_session_ticks_until_the_resume_grace_elapses() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        a.observe(obs(T0), &c);
        a.observe(obs(T0 + 7.0), &c);
        a.observe(obs(T0 + 16.0), &c);
        assert_eq!(a.kind(), ActivityKind::Waiting);
        assert_eq!(a.next_tick(&c, T0 + 16.0), None, "red and quiet: no timer");

        // Work resumes: a busy streak opens and must be watched to clear the dot.
        let t = T0 + 20.0;
        a.note_output(t);
        a.observe(obs(t), &c);
        assert_eq!(a.kind(), ActivityKind::Waiting, "red is sticky");
        let d = a.next_tick(&c, t).expect("a resume streak must tick");
        assert!(d <= c.resume_grace() + 1e-9);
        a.note_output(t + d);
        assert_eq!(a.observe(obs(t + d), &c), ActivityKind::Active);
    }

    #[test]
    fn cpu_busy_is_honoured_even_with_no_output() {
        let c = cfg();
        let mut a = started();
        let o = Observation {
            cpu_busy: true,
            ..obs(T0)
        };
        assert_eq!(a.observe(o, &c), ActivityKind::Active);
    }

    #[test]
    fn a_deadline_in_the_past_still_yields_a_real_sleep() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        a.observe(obs(T0), &c);
        // Ask long after the stamp went stale.
        let d = a.next_tick(&c, T0 + 100.0);
        assert!(d.is_none_or(|d| d >= MIN_TICK_SECS), "never a spin: {d:?}");
    }

    #[test]
    fn state_since_reports_the_streak_start() {
        let c = cfg();
        let mut a = started();
        a.note_output(T0);
        a.observe(obs(T0), &c);
        assert_eq!(a.state_since(), Some(T0), "busy: the last active stamp");
        a.observe(obs(T0 + 7.0), &c);
        assert_eq!(
            a.state_since(),
            Some(T0 + 7.0),
            "quiet: the honest waiting-since"
        );
    }

    // ── the shared unsolicited rule ──────────────────────────────────────────

    #[test]
    fn unsolicited_age_matches_the_documented_rule() {
        let c = cfg();
        // No output at all.
        assert_eq!(unsolicited_age(None, None, Some(100.0), &c), None);
        // Inside the spawn grace.
        assert_eq!(unsolicited_age(Some(0.5), None, Some(1.0), &c), None);
        // No session age known yet.
        assert_eq!(unsolicited_age(Some(0.5), None, None, &c), None);
        // Echo: input 0.5s older than the output, inside the 1.0s gap.
        assert_eq!(unsolicited_age(Some(1.0), Some(1.5), Some(100.0), &c), None);
        // Input well older than the output: genuine work.
        assert_eq!(
            unsolicited_age(Some(1.0), Some(9.0), Some(100.0), &c),
            Some(1.0)
        );
        // No input ever: genuine work.
        assert_eq!(unsolicited_age(Some(1.0), None, Some(100.0), &c), Some(1.0));
    }

    #[test]
    fn unsolicited_rule_honours_the_config_knobs() {
        // Zero gap disables echo suppression; zero grace trusts output at once.
        let c = ActivityConfig {
            unsolicited_gap_secs: 0.0,
            spawn_grace_secs: 0.0,
            ..Default::default()
        };
        assert_eq!(
            unsolicited_age(Some(1.0), Some(1.0001), Some(0.0), &c),
            Some(1.0),
            "a zero gap must not swallow output"
        );
        // A generous grace suppresses output a default config would count.
        let strict = ActivityConfig {
            spawn_grace_secs: 60.0,
            ..Default::default()
        };
        assert_eq!(unsolicited_age(Some(1.0), None, Some(30.0), &strict), None);
    }
}
