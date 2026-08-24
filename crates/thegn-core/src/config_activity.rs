//! `[activity]` config — the sidebar activity dots' tuning knobs, split out of
//! `config.rs` (the god-file ratchet) like `config_daemon` / `config_theme`.
//!
//! Every value here used to be a hard-coded `const` in [`crate::activity`], so
//! the machine that reported a dot as "glitchy" had no way to tune it and
//! `openspec/specs/platform-windows/spec.md` promised a "configured cooldown"
//! that did not exist. The [`Default`] impl is deliberately the *legacy*
//! constant set except where the audit changed a default on purpose (see
//! `quiet_grace_secs`), so `ActivityConfig::default()` is the documented
//! behaviour rather than a second, drifting source of truth.
//!
//! Accessors, not raw fields, are what the FSM reads: a hand-edited TOML can
//! carry a zero or a negative grace, and a state machine that divides by or
//! compares against those produces exactly the flapping this section exists to
//! stop. Every getter clamps; the raw field stays public so `thegn config` can
//! round-trip what the user actually wrote.

use serde::{Deserialize, Serialize};

/// Agent CLI program names that count as "an agent is running here" when no
/// `[[agents]]` entry and no DB-bound agent names them. Kept as a default list
/// rather than a hard-coded predicate so a user running something exotic can add
/// it without a rebuild (`[activity] agent_programs`).
fn default_agent_programs() -> Vec<String> {
    [
        "claude",
        "codex",
        "aider",
        "gemini",
        "opencode",
        "cursor-agent",
        "amp",
        "crush",
        "goose",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `[activity]` — how the sidebar's activity dots decide that a worktree is
/// working, has finished, or needs you.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ActivityConfig {
    /// Track activity at all. `false` ⇒ no dots and no background probe.
    pub enabled: bool,
    /// CPU that counts as "working", as a percentage of one core. The scan sums
    /// per-process CPU time under each worktree, so this is the whole worktree's
    /// share, not one process's.
    pub cpu_percent: f64,
    /// A single process holding at least this fraction of ONE core, continuously,
    /// is a runaway candidate. `0` disables the check.
    ///
    /// Distinct from `cpu_percent`, which is the whole worktree's share and asks
    /// "is work happening here". This asks the opposite question — "is one
    /// process doing nothing but burn a core, indefinitely" — which no existing
    /// signal answered: an `sh -c "while :; do :; done"` held a core for four
    /// days (84 core-hours) while every dot and threshold behaved normally,
    /// because from the outside it is indistinguishable from a long build.
    /// Duration is what separates them, hence `runaway_secs`.
    pub runaway_core_fraction: f64,
    /// How long a process must hold `runaway_core_fraction` *continuously*
    /// before it is reported. `0` disables the check.
    ///
    /// Deliberately long: a compile, a test suite and a video encode all peg a
    /// core legitimately, and a report that fires on those is one you learn to
    /// ignore. An hour is well past any of them.
    pub runaway_secs: f64,
    /// How long a working worktree must stay quiet before its dot turns
    /// "finished / needs you". A *confirming* observation is always also
    /// required, so the real latency is this plus one poll interval.
    ///
    /// Raised from the legacy 5.0: that value equalled the poll cadence, so the
    /// grace aliased away entirely and a single quiet window flipped the dot —
    /// the "turns red while the agent is still working" bug.
    pub quiet_grace_secs: f64,
    /// How long a finished/blocked worktree must be *continuously* busy before
    /// its dot goes back to working. Guards against a single spinner redraw or
    /// stray watcher blip clearing a dot the user hasn't seen.
    pub resume_grace_secs: f64,
    /// Ignore pane output younger than this: spawn banners, prompt paint and a
    /// reattach's scrollback replay are not live agent work.
    pub spawn_grace_secs: f64,
    /// Output this soon after a keystroke into the same pane is echo (or an
    /// immediate command response), not unsolicited agent work.
    pub unsolicited_gap_secs: f64,
    /// Floor on the output-freshness window. The window is normally the elapsed
    /// poll interval, which a coalesced or rate-limited poll can compress to
    /// almost nothing — dropping a legitimately fresh stamp on the floor.
    pub output_hint_ttl_secs: f64,
    /// Only a worktree with a real agent may show a "needs you" dot. A plain
    /// shell that ran a command and went quiet returns to no dot instead of
    /// latching a red alert forever.
    pub red_requires_agent: bool,
    /// Agent program names recognized in addition to `[[agents]]` entries and
    /// each worktree's DB-bound agent.
    pub agent_programs: Vec<String>,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_percent: 3.0,
            runaway_core_fraction: 0.9,
            runaway_secs: 3600.0,
            quiet_grace_secs: 8.0,
            resume_grace_secs: 3.0,
            spawn_grace_secs: 5.0,
            unsolicited_gap_secs: 1.0,
            output_hint_ttl_secs: 6.0,
            red_requires_agent: true,
            agent_programs: default_agent_programs(),
        }
    }
}

/// Lower bound for every grace/TTL. A zero or negative window makes the FSM's
/// `>=` comparisons fire on the same observation that armed them, which is
/// indistinguishable from having no state machine at all.
const MIN_SECS: f64 = 0.1;

impl ActivityConfig {
    /// CPU time per wall-second that counts as busy, in the jiffy units the scan
    /// reports (`CLK_TCK` = 100, so 1% of a core = 1 jiffy/s). Clamped to a sane
    /// band: `0` would make *any* sample busy forever, and a value over 100%
    /// per core would need every core pinned to register.
    pub fn active_jiffies_per_sec(&self) -> f64 {
        self.cpu_percent.clamp(0.1, 100.0)
    }

    /// [`Self::quiet_grace_secs`], clamped.
    pub fn quiet_grace(&self) -> f64 {
        self.quiet_grace_secs.max(MIN_SECS)
    }

    /// [`Self::resume_grace_secs`], clamped.
    pub fn resume_grace(&self) -> f64 {
        self.resume_grace_secs.max(MIN_SECS)
    }

    /// [`Self::spawn_grace_secs`], clamped. `0` is meaningful here — it means
    /// "trust output immediately" — so this floors at zero, not `MIN_SECS`.
    pub fn spawn_grace(&self) -> f64 {
        self.spawn_grace_secs.max(0.0)
    }

    /// [`Self::unsolicited_gap_secs`], clamped. `0` disables echo suppression.
    pub fn unsolicited_gap(&self) -> f64 {
        self.unsolicited_gap_secs.max(0.0)
    }

    /// [`Self::output_hint_ttl_secs`], clamped.
    pub fn output_hint_ttl(&self) -> f64 {
        self.output_hint_ttl_secs.max(MIN_SECS)
    }

    /// Whether `program` is a recognized agent CLI name (case-insensitive).
    /// Only the configured list — callers OR this with the `[[agents]]` commands
    /// and the worktree's own bound agent, which they know and this type does
    /// not.
    pub fn is_agent_program(&self, program: &str) -> bool {
        !program.is_empty()
            && self
                .agent_programs
                .iter()
                .any(|a| a.eq_ignore_ascii_case(program))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_behaviour() {
        let a = ActivityConfig::default();
        assert!(a.enabled);
        assert!(a.red_requires_agent, "a bare shell must never go red");
        // The legacy constants that the audit kept as-is.
        assert_eq!(a.cpu_percent, 3.0);
        assert_eq!(a.resume_grace_secs, 3.0);
        assert_eq!(a.spawn_grace_secs, 5.0);
        assert_eq!(a.unsolicited_gap_secs, 1.0);
        // The one default the audit deliberately changed: 5.0 aliased to the
        // poll cadence, so the grace never actually damped anything.
        assert_eq!(a.quiet_grace_secs, 8.0);
        // 1% of a core == 1 jiffy/s, so cpu_percent passes through unscaled.
        assert_eq!(a.active_jiffies_per_sec(), 3.0);
    }

    #[test]
    fn graces_clamp_away_zero_and_negative_windows() {
        let a = ActivityConfig {
            quiet_grace_secs: 0.0,
            resume_grace_secs: -5.0,
            output_hint_ttl_secs: 0.0,
            cpu_percent: 0.0,
            ..Default::default()
        };
        assert_eq!(a.quiet_grace(), MIN_SECS);
        assert_eq!(a.resume_grace(), MIN_SECS);
        assert_eq!(a.output_hint_ttl(), MIN_SECS);
        assert_eq!(a.active_jiffies_per_sec(), 0.1);
        // An absurd CPU threshold is capped rather than making the dot dead.
        let hot = ActivityConfig {
            cpu_percent: 100_000.0,
            ..Default::default()
        };
        assert_eq!(hot.active_jiffies_per_sec(), 100.0);
    }

    #[test]
    fn zero_is_honoured_for_the_suppression_windows() {
        // Unlike the graces, "no spawn grace" / "no echo window" are coherent
        // requests: they mean trust every byte immediately.
        let a = ActivityConfig {
            spawn_grace_secs: 0.0,
            unsolicited_gap_secs: 0.0,
            ..Default::default()
        };
        assert_eq!(a.spawn_grace(), 0.0);
        assert_eq!(a.unsolicited_gap(), 0.0);
        // Negative still clamps to zero.
        let neg = ActivityConfig {
            spawn_grace_secs: -1.0,
            unsolicited_gap_secs: -1.0,
            ..Default::default()
        };
        assert_eq!(neg.spawn_grace(), 0.0);
        assert_eq!(neg.unsolicited_gap(), 0.0);
    }

    #[test]
    fn agent_programs_match_case_insensitively_and_reject_empty() {
        let a = ActivityConfig::default();
        assert!(a.is_agent_program("claude"));
        assert!(a.is_agent_program("CLAUDE"));
        assert!(a.is_agent_program("cursor-agent"));
        // Not an agent: a shell, a tool drawer, an empty (unresolved) program.
        assert!(!a.is_agent_program("zsh"));
        assert!(!a.is_agent_program("htop"));
        assert!(!a.is_agent_program(""));
        // The list is user-extensible.
        let custom = ActivityConfig {
            agent_programs: vec!["my-agent".into()],
            ..Default::default()
        };
        assert!(custom.is_agent_program("my-agent"));
        assert!(!custom.is_agent_program("claude"));
    }
}
