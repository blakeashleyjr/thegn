//! Vercel lifetime reaper, on the hydration cadence beside `fly_reaper`: at
//! `max_lifetime_secs` a sandbox is STOPPED (compute to $0) and its ledger
//! record marked expired; it is deleted (with its snapshots) only after the
//! grace period, or at once when its worktree is gone. Never creates or
//! resumes. Inert with no vercel env (an early-return config scan).

use thegn_core::config::Config;

/// One reaper pass. Spine stub, filled by lane 7.
pub fn tick(_cfg: &Config) {}
