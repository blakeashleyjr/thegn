//! The activity FSM's per-worktree transition, lifted out of
//! [`crate::activity`]'s `poll` so the edges are testable without touching the
//! filesystem, `/proc`, or a CPU-burner subprocess.
//!
//! `poll` still owns *observation* (the elapsed wall window, the CPU threshold,
//! the `/proc` scan, persistence); this module owns the *decision*. Everything
//! here is pure: two structs in, one struct out, clock injected.
//!
//! Two changes to the old inline logic, both fixing reported bugs:
//!
//! **Confirmed quiet.** `quiet_since` used to mean "when we flipped to red";
//! it now means "when the current quiet streak began" — the exact mirror of
//! `busy_since` on the resume edge. Since `quiet_since` can only have been set
//! by an *earlier* step, arming red inherently needs two consecutive non-busy
//! observations **plus** the grace to elapse. The old edge compared
//! `now - last_active_at` against a grace that happened to equal the poll
//! cadence, so `now - last_active_at` was already at the threshold on the very
//! next poll: one quiet window flipped the dot and the grace damped nothing.
//! That is the "turns red while the agent is still working" bug, and it also
//! deletes the `last_active_at.unwrap_or(0.0)` trap (an `active` entry with no
//! stamp measured its idleness from the epoch and flipped instantly), because
//! this edge no longer reads `last_active_at` at all.
//!
//! **Agentness.** Red means "an agent needs you". A worktree with no agent goes
//! white while it genuinely burns CPU and then back to *no dot* — it never
//! latches a red alert. Without this, any CPU under the worktree path (a
//! `git status`, `direnv`, an LSP, shell autosuggestions — the threshold is
//! ~3% of one core) armed a permanent "look at me" dot on a bare terminal.
//! [`Agentness::Unknown`] keeps the legacy behaviour for a worktree we have no
//! evidence about either way, so a session-overlay worktree with no DB row yet
//! is never wrongly healed.
//!
//! Note that `active` itself is deliberately *not* agent-gated: `lifecycle` and
//! `hibernator` both read it as "don't drop the bridge / don't hibernate", so
//! gating it would suspend a VM out from under a `cargo build` in a shell pane.

use crate::config_activity::ActivityConfig;

/// Tolerance for a stamp that reads as *newer* than now — small clock steps
/// between the loop stamping output and the poll judging it. A stamp further in
/// the future than this is garbage and must not pin a worktree busy forever.
const FUTURE_SLACK_SECS: f64 = 1.0;

/// What we know about whether a real agent is running in a worktree.
///
/// Three-valued on purpose. `Absent` is a positive claim ("we looked, there is
/// no agent here") and is the only value that suppresses red; `Unknown` means we
/// have no row for this worktree yet and behaves exactly as the FSM did before
/// the gate existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Agentness {
    #[default]
    Unknown,
    Present,
    Absent,
}

impl Agentness {
    /// Read agentness out of a `worktree path -> has-real-agent` map. A missing
    /// key is [`Agentness::Unknown`], not `Absent` — absence of evidence is not
    /// evidence of absence, and treating it as `Absent` would silently clear
    /// genuine red dots for any worktree the caller hadn't classified yet.
    pub fn from_map(map: &std::collections::BTreeMap<String, bool>, worktree: &str) -> Self {
        match map.get(worktree) {
            Some(true) => Self::Present,
            Some(false) => Self::Absent,
            None => Self::Unknown,
        }
    }
}

/// The observations one step folds in.
#[derive(Debug, Clone, Copy)]
pub struct Signals {
    /// CPU over the threshold, or fresh unsolicited agent-pane output.
    pub busy: bool,
    pub agent: Agentness,
}

/// The mutable bookkeeping a worktree's entry carries between steps. Mirrors the
/// persisted `Entry` fields the transition touches, so `poll` can move them in
/// and out without this module knowing about serde or the snapshot format.
#[derive(Debug, Clone, PartialEq)]
pub struct Marks {
    /// `"none" | "active" | "waiting" | "read"` (plus legacy/unknown strings,
    /// which are treated as `"none"`).
    pub state: String,
    /// When the current *quiet* streak began; `None` while busy.
    pub quiet_since: Option<f64>,
    /// When the current *busy* streak began; `None` while quiet.
    pub busy_since: Option<f64>,
    /// Last step at which this worktree was observed busy.
    pub last_active_at: Option<f64>,
}

/// Whether an output stamp counts as "output within the window we just observed".
///
/// The window is normally the elapsed poll interval, but a coalesced or
/// rate-limited poll can compress that to almost nothing, dropping a
/// legitimately fresh stamp on the floor — so it is floored at
/// `output_hint_ttl`. A stamp in the future beyond [`FUTURE_SLACK_SECS`] is
/// rejected so clock skew or garbage can't pin a worktree busy forever.
pub fn output_is_fresh(stamp: f64, now: f64, wall: f64, cfg: &ActivityConfig) -> bool {
    let window = wall.max(cfg.output_hint_ttl());
    let age = now - stamp;
    (-FUTURE_SLACK_SECS..=window + FUTURE_SLACK_SECS).contains(&age)
}

/// Advance one worktree's marks by a single observation.
pub fn step(mut m: Marks, sig: Signals, cfg: &ActivityConfig, now: f64) -> Marks {
    // Track the uninterrupted streaks. Each observation belongs to exactly one
    // of them, so entering a streak clears the other's start.
    if sig.busy {
        m.busy_since.get_or_insert(now);
        m.quiet_since = None;
    } else {
        m.busy_since = None;
    }

    // A worktree we positively know has no agent may never hold a red dot.
    let no_agent = cfg.red_requires_agent && sig.agent == Agentness::Absent;

    match m.state.as_str() {
        // Red is sticky: only sustained, genuine work resumes it, so a momentary
        // blip from a stray watcher can't clear an alert the user hasn't seen.
        "waiting" | "read" => {
            if no_agent && !sig.busy {
                // Self-heal a red dot this worktree should never have had —
                // inherited from a recycled tab, or armed before we learned the
                // worktree has no agent.
                return settled(m);
            }
            if sig.busy && now - m.busy_since.unwrap_or(now) >= cfg.resume_grace() {
                m.state = "active".into();
                m.quiet_since = None;
                m.last_active_at = Some(now);
            }
        }
        "active" => {
            if sig.busy {
                m.last_active_at = Some(now);
            } else {
                // `get_or_insert` is what makes this a *confirmed* quiet edge:
                // the first quiet observation only opens the streak (age 0), so
                // the flip can never happen on the same step that noticed it.
                let since = *m.quiet_since.get_or_insert(now);
                if now - since >= cfg.quiet_grace() {
                    if no_agent {
                        return settled(m);
                    }
                    m.state = "waiting".into();
                    // Leave `quiet_since` at the streak start: it is the honest
                    // "waiting since" the attention model ranks and acks on.
                }
            }
        }
        // none / legacy / unknown: any work wakes it. Not agent-gated — a white
        // dot claims only "something is running here", which is true.
        _ => {
            if sig.busy {
                m.state = "active".into();
                m.quiet_since = None;
                m.last_active_at = Some(now);
            }
        }
    }
    m
}

/// The dot-less resting state, with both streaks cleared so the next step starts
/// from a clean slate. `last_active_at` is carried through — it is the recency
/// key the sidebar's `Live` sort orders by, and dropping it would reshuffle rows
/// every time a shell worktree settled.
fn settled(m: Marks) -> Marks {
    Marks {
        state: "none".into(),
        quiet_since: None,
        busy_since: None,
        last_active_at: m.last_active_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ActivityConfig {
        ActivityConfig::default()
    }

    fn marks(state: &str) -> Marks {
        Marks {
            state: state.into(),
            quiet_since: None,
            busy_since: None,
            last_active_at: None,
        }
    }

    fn sig(busy: bool, agent: Agentness) -> Signals {
        Signals { busy, agent }
    }

    // ── the "red while still working" bug ────────────────────────────────────

    /// A single quiet observation must never arm red, however long the poll
    /// window was. This is the regression test for the reported bug: the old
    /// edge compared `now - last_active_at` to a grace equal to the poll
    /// cadence, so one quiet window was enough.
    #[test]
    fn single_quiet_poll_never_flips_active_to_red() {
        let m = Marks {
            state: "active".into(),
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        // A 60s gap — far past any grace — still only *opens* the quiet streak.
        let out = step(m, sig(false, Agentness::Present), &cfg(), 1060.0);
        assert_eq!(
            out.state, "active",
            "one quiet observation must not arm red"
        );
        assert_eq!(out.quiet_since, Some(1060.0), "the streak starts here");
    }

    #[test]
    fn quiet_needs_a_confirming_poll_then_arms_red() {
        let c = cfg();
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Present), &c, 1001.0);
        assert_eq!(m.state, "active");
        // Still inside the grace measured from the streak start.
        m = step(m, sig(false, Agentness::Present), &c, 1005.0);
        assert_eq!(m.state, "active");
        // Grace elapsed since the streak began (1001 + 8.0).
        m = step(m, sig(false, Agentness::Present), &c, 1009.0);
        assert_eq!(m.state, "waiting");
        assert_eq!(
            m.quiet_since,
            Some(1001.0),
            "quiet_since is the streak start, the honest `waiting since`"
        );
    }

    #[test]
    fn busy_blip_resets_the_quiet_streak() {
        let c = cfg();
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Present), &c, 1001.0);
        assert_eq!(m.quiet_since, Some(1001.0));
        // One busy observation mid-streak: the agent is alive after all.
        m = step(m, sig(true, Agentness::Present), &c, 1002.0);
        assert_eq!(m.quiet_since, None, "a busy observation clears the streak");
        assert_eq!(m.last_active_at, Some(1002.0));
        // The clock is now well past the original streak start + grace, but the
        // streak restarted, so red is not armed.
        m = step(m, sig(false, Agentness::Present), &c, 1003.0);
        assert_eq!(m.state, "active");
        assert_eq!(m.quiet_since, Some(1003.0));
    }

    /// An `active` entry with no `last_active_at` (an older writer's snapshot)
    /// used to measure its idleness from the epoch and flip red immediately.
    #[test]
    fn quiet_flip_survives_a_missing_last_active_stamp() {
        let out = step(
            marks("active"),
            sig(false, Agentness::Present),
            &cfg(),
            1_700_000_000.0,
        );
        assert_eq!(
            out.state, "active",
            "a missing stamp must not read as infinitely idle"
        );
    }

    // ── the "bare terminal goes red" bug ─────────────────────────────────────

    /// The reported bug: a shell worktree that ran something and went quiet must
    /// return to no dot, not latch a red "look at me".
    #[test]
    fn non_agent_active_settles_to_none_not_red() {
        let c = cfg();
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Absent), &c, 1001.0);
        assert_eq!(m.state, "active", "still busy-adjacent, dot stays white");
        m = step(m, sig(false, Agentness::Absent), &c, 1010.0);
        assert_eq!(m.state, "none", "a bare shell never goes red");
        assert_eq!(m.quiet_since, None);
        assert_eq!(m.busy_since, None);
        assert_eq!(
            m.last_active_at,
            Some(1000.0),
            "settling keeps the recency key the `Live` sort orders by"
        );
    }

    /// A red dot inherited by a worktree we now know has no agent (a recycled
    /// tab name, or a dot armed before the agent map was populated) heals.
    #[test]
    fn non_agent_red_self_heals_to_none() {
        for state in ["waiting", "read"] {
            let m = Marks {
                quiet_since: Some(900.0),
                ..marks(state)
            };
            let out = step(m, sig(false, Agentness::Absent), &cfg(), 1000.0);
            assert_eq!(out.state, "none", "{state} must heal for a shell worktree");
        }
    }

    /// A busy non-agent worktree still shows the white "something is running"
    /// dot — `lifecycle`/`hibernator` depend on `active` meaning exactly that.
    #[test]
    fn non_agent_cpu_still_arms_active() {
        let out = step(marks("none"), sig(true, Agentness::Absent), &cfg(), 1000.0);
        assert_eq!(out.state, "active");
        assert_eq!(out.last_active_at, Some(1000.0));
    }

    #[test]
    fn agent_present_still_arms_red() {
        let c = cfg();
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Present), &c, 1001.0);
        m = step(m, sig(false, Agentness::Present), &c, 1020.0);
        assert_eq!(m.state, "waiting");
    }

    /// No evidence either way ⇒ the pre-gate behaviour, so a worktree the caller
    /// hasn't classified yet keeps working dots.
    #[test]
    fn unknown_agentness_keeps_legacy_behaviour() {
        let c = cfg();
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Unknown), &c, 1001.0);
        m = step(m, sig(false, Agentness::Unknown), &c, 1020.0);
        assert_eq!(m.state, "waiting", "Unknown must not suppress red");
        // And an Unknown red dot is not healed away.
        let held = step(m, sig(false, Agentness::Unknown), &c, 1030.0);
        assert_eq!(held.state, "waiting");
    }

    /// Turning the gate off restores the old (agent-blind) behaviour wholesale.
    #[test]
    fn disabling_the_gate_lets_a_shell_go_red_again() {
        let c = ActivityConfig {
            red_requires_agent: false,
            ..Default::default()
        };
        let mut m = Marks {
            last_active_at: Some(1000.0),
            ..marks("active")
        };
        m = step(m, sig(false, Agentness::Absent), &c, 1001.0);
        m = step(m, sig(false, Agentness::Absent), &c, 1020.0);
        assert_eq!(m.state, "waiting");
    }

    // ── the resume edge (unchanged semantics) ────────────────────────────────

    #[test]
    fn red_resumes_only_after_sustained_busy() {
        let c = cfg();
        for state in ["waiting", "read"] {
            let mut m = Marks {
                quiet_since: Some(990.0),
                ..marks(state)
            };
            // First busy observation opens the streak but must not clear red.
            m = step(m, sig(true, Agentness::Present), &c, 1000.0);
            assert_eq!(m.state, state, "a single busy window must not clear red");
            assert_eq!(m.busy_since, Some(1000.0));
            // Still under the resume grace.
            m = step(m, sig(true, Agentness::Present), &c, 1002.0);
            assert_eq!(m.state, state);
            // Sustained past it.
            m = step(m, sig(true, Agentness::Present), &c, 1003.5);
            assert_eq!(m.state, "active");
            assert_eq!(m.quiet_since, None);
            assert_eq!(m.last_active_at, Some(1003.5));
        }
    }

    #[test]
    fn a_busy_gap_restarts_the_resume_streak() {
        let c = cfg();
        let mut m = marks("waiting");
        m = step(m, sig(true, Agentness::Present), &c, 1000.0);
        // Quiet observation drops the streak entirely.
        m = step(m, sig(false, Agentness::Present), &c, 1001.0);
        assert_eq!(m.busy_since, None);
        assert_eq!(m.state, "waiting", "still red, and the streak is gone");
        // So a later busy observation starts counting from scratch.
        m = step(m, sig(true, Agentness::Present), &c, 1002.0);
        assert_eq!(m.busy_since, Some(1002.0));
        assert_eq!(m.state, "waiting");
    }

    /// A red dot with an *unknown* agentness holds through a quiet step rather
    /// than healing — the sticky contract the FSM has always had.
    #[test]
    fn red_is_sticky_for_agent_bearing_worktrees() {
        let out = step(
            Marks {
                quiet_since: Some(900.0),
                ..marks("waiting")
            },
            sig(false, Agentness::Present),
            &cfg(),
            5000.0,
        );
        assert_eq!(out.state, "waiting");
        assert_eq!(out.quiet_since, Some(900.0), "the waiting-since is stable");
    }

    // ── legacy states + the wake edge ────────────────────────────────────────

    #[test]
    fn unknown_and_legacy_states_wake_on_work() {
        for state in ["none", "quiet", "running", "gibberish", ""] {
            let out = step(marks(state), sig(true, Agentness::Present), &cfg(), 1000.0);
            assert_eq!(out.state, "active", "{state:?} should wake to active");
        }
    }

    #[test]
    fn idle_none_stays_none_and_records_nothing() {
        let out = step(marks("none"), sig(false, Agentness::Absent), &cfg(), 1000.0);
        assert_eq!(out.state, "none");
        assert_eq!(out.last_active_at, None);
        assert_eq!(out.busy_since, None);
    }

    // ── agentness lookup ────────────────────────────────────────────────────

    #[test]
    fn agentness_from_map_treats_a_missing_key_as_unknown() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("/wt/agent".to_string(), true);
        map.insert("/wt/shell".to_string(), false);
        assert_eq!(Agentness::from_map(&map, "/wt/agent"), Agentness::Present);
        assert_eq!(Agentness::from_map(&map, "/wt/shell"), Agentness::Absent);
        assert_eq!(Agentness::from_map(&map, "/wt/new"), Agentness::Unknown);
        assert_eq!(Agentness::default(), Agentness::Unknown);
    }

    // ── output freshness ────────────────────────────────────────────────────

    #[test]
    fn output_freshness_is_floored_at_the_ttl() {
        let c = cfg(); // output_hint_ttl = 6.0, future slack = 1.0
        // A poll whose window collapsed to 0.2s still accepts a 4s-old stamp,
        // because the floor (6.0) governs rather than the compressed window.
        assert!(output_is_fresh(996.0, 1000.0, 0.2, &c));
        // Past the floor + slack (age 8 > 6 + 1) it is stale.
        assert!(!output_is_fresh(992.0, 1000.0, 0.2, &c));
        // A long window governs when it exceeds the floor: age 10 <= 10 + 1.
        assert!(output_is_fresh(990.0, 1000.0, 10.0, &c));
        assert!(!output_is_fresh(988.0, 1000.0, 10.0, &c));
    }

    #[test]
    fn output_freshness_rejects_a_far_future_stamp() {
        let c = cfg();
        // Small skew is tolerated (the loop stamps, the poll judges later).
        assert!(output_is_fresh(1000.5, 1000.0, 5.0, &c));
        // Garbage from the future must not pin a worktree busy forever.
        assert!(!output_is_fresh(1050.0, 1000.0, 5.0, &c));
    }
}
