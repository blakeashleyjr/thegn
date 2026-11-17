//! The daemon's `schedule_due` producer (THE-2193, filled by b153 lane 2).
//!
//! A sibling of the `worktree_idle` timer in `subscribe_daemon_events`: one
//! daemon task, spawned only when at least one enabled rule has a schedule,
//! sleeping until the single earliest due time (`automation_schedule::next_due`)
//! and submitting `automation_schedule::schedule_event` through
//! `automation_runtime::submit`. It exits when no scheduled rule remains.
//! `rearm` (called by `automations.put`/`rm`/`enable`) respawns or nudges it;
//! `armed` keeps `idle_exit_loop` from reaping a daemon that has cron work.
//! The UI process never produces `schedule_due`.

use thegn_core::config::Config;

/// Start the producer if any enabled rule has a schedule. Idle invariant: with
/// no scheduled rule this spawns nothing.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 2")]
pub(crate) fn spawn(cfg: &Config) {}

/// Re-read the rule set after a rule write and respawn or nudge the producer.
#[allow(dead_code, reason = "filled by b153 lane 2")]
pub(crate) fn rearm() {}

/// Whether a schedule producer is live (the daemon is not idle while it is).
#[allow(dead_code, reason = "filled by b153 lane 2")]
pub(crate) fn armed() -> bool {
    false
}
