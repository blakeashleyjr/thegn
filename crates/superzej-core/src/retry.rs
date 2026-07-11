//! Retry/backoff policy shared by every remote-transport recovery loop: the
//! pane reconnect wrapper, the host bring-up control plane, delivery steps,
//! and the sandbox runtime probe. Pure — callers own the sleeping.
//!
//! Relocated from `placement.rs` (where it was pane-specific) when the
//! flaky-link hardening taught the control plane to retry too.

use crate::transport_error::{ClassifiedErr, ErrorClass};
use std::time::{Duration, Instant};

/// Reconnect/retry policy for a dropped remote transport. Pure — decides
/// whether another attempt is warranted and the backoff before it, so callers
/// can wrap any remote operation in a retry loop without hard-coding cadence.
/// mosh self-heals roaming; this covers ssh/exec paths where the channel drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        ReconnectPolicy {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 10_000,
        }
    }
}

/// The installed `[remote]` control-plane policy (first set wins — see
/// `config_remote::RemoteConfig::install`).
static CONTROL_PLANE: std::sync::OnceLock<ReconnectPolicy> = std::sync::OnceLock::new();

/// Install the resolved control-plane retry policy from `[remote]` config.
pub fn set_control_plane(p: ReconnectPolicy) {
    let _ = CONTROL_PLANE.set(p); // best-effort: first-set-wins by design
}

impl ReconnectPolicy {
    /// The control-plane profile: host bring-up steps (connect/probe/resolve)
    /// on a flaky link. Reads the installed `[remote]` tuning; defaults apply
    /// before config load (and in tests).
    pub fn control_plane() -> Self {
        CONTROL_PLANE
            .get()
            .copied()
            .unwrap_or_else(Self::control_plane_default)
    }

    /// The built-in control-plane profile. Slower base than the pane profile —
    /// each attempt is a whole ssh exec with its own ConnectTimeout, so
    /// rapid-fire retries just burn the budget while the link is still down.
    pub fn control_plane_default() -> Self {
        ReconnectPolicy {
            max_attempts: 4,
            base_delay_ms: 1_000,
            max_delay_ms: 15_000,
        }
    }

    /// The sandbox runtime-probe profile: short (a worktree create is waiting
    /// on it) but enough to ride out a one-off flap.
    pub fn probe() -> Self {
        ReconnectPolicy {
            max_attempts: 3,
            base_delay_ms: 1_000,
            max_delay_ms: 4_000,
        }
    }

    /// Exponential backoff before `attempt` (1-based), capped at `max_delay_ms`;
    /// `None` once attempts are exhausted (`attempt > max_attempts`) or zero.
    pub fn backoff(&self, attempt: u32) -> Option<std::time::Duration> {
        if attempt == 0 || attempt > self.max_attempts {
            return None;
        }
        let shift = (attempt - 1).min(20);
        let ms = self
            .base_delay_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_delay_ms);
        Some(std::time::Duration::from_millis(ms))
    }

    /// Whether a remote pane that exited with `exit_code` on its `attempt`-th
    /// run should reconnect. ssh/mosh report a connection drop as 255; a clean
    /// or application exit (anything else) is terminal — the user quit the shell.
    pub fn should_reconnect(&self, exit_code: i32, attempt: u32) -> bool {
        attempt <= self.max_attempts && exit_code == 255
    }

    /// Whether a classified control-plane failure after `completed` attempts
    /// (1-based) warrants another try: only transient errors, and only while
    /// the total try count stays within `max_attempts`.
    pub fn should_retry(&self, class: ErrorClass, completed: u32) -> bool {
        class == ErrorClass::Transient && completed < self.max_attempts
    }
}

/// A retry budget for one bring-up/delivery step: the per-attempt policy plus
/// an overall wall-clock cap so a slow-failing op (each attempt eating its full
/// ConnectTimeout) can't stretch a step unboundedly.
#[derive(Debug, Clone, Copy)]
pub struct StepBudget {
    pub policy: ReconnectPolicy,
    pub overall: Duration,
}

impl StepBudget {
    pub fn new(policy: ReconnectPolicy, overall: Duration) -> Self {
        StepBudget { policy, overall }
    }
}

/// Run `op` with bounded, classified retries.
///
/// * Transient failures retry per `budget.policy` (exponential backoff) until
///   attempts or the overall wall-clock budget run out.
/// * Permanent failures short-circuit immediately.
/// * `note` receives a human line before each retry ("retrying connect (2/4)
///   in 2s — ssh transport dropped …") — wire it to the provisioning board.
/// * `between` runs before each retry — the master-hygiene hook (reset a
///   wedged ControlMaster so the next attempt builds a fresh connection).
///
/// The final error is annotated with the attempt count when retries happened,
/// so a persisted failure reads "… (after 4 attempts)" — evidence the ladder
/// ran, not a one-blip verdict.
pub fn with_retry<T>(
    label: &str,
    budget: &StepBudget,
    note: &mut dyn FnMut(String),
    between: &mut dyn FnMut(),
    op: &mut dyn FnMut() -> Result<T, ClassifiedErr>,
) -> Result<T, ClassifiedErr> {
    let start = Instant::now();
    let mut attempt: u32 = 1;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let give_up = |e: ClassifiedErr, attempts: u32| {
                    if attempts > 1 {
                        ClassifiedErr {
                            class: e.class,
                            msg: format!("{} (after {attempts} attempts)", e.msg),
                        }
                    } else {
                        e
                    }
                };
                if !budget.policy.should_retry(e.class, attempt) {
                    return Err(give_up(e, attempt));
                }
                let Some(delay) = budget.policy.backoff(attempt) else {
                    return Err(give_up(e, attempt));
                };
                if start.elapsed() + delay >= budget.overall {
                    return Err(give_up(e, attempt));
                }
                note(format!(
                    "retrying {label} ({attempt}/{}) in {}s — {}",
                    budget.policy.max_attempts,
                    delay.as_secs_f32().max(0.0).round() as u64,
                    e.msg
                ));
                between();
                std::thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_capped() {
        let p = ReconnectPolicy::default(); // 5 × 500ms..10s
        assert_eq!(p.backoff(0), None, "attempt is 1-based");
        assert_eq!(p.backoff(1).unwrap().as_millis(), 500);
        assert_eq!(p.backoff(2).unwrap().as_millis(), 1000);
        assert_eq!(p.backoff(3).unwrap().as_millis(), 2000);
        assert_eq!(p.backoff(5).unwrap().as_millis(), 8000);
        assert_eq!(p.backoff(6), None, "budget exhausted");
        let capped = ReconnectPolicy {
            max_attempts: 30,
            base_delay_ms: 500,
            max_delay_ms: 3_000,
        };
        assert_eq!(capped.backoff(10).unwrap().as_millis(), 3000, "cap holds");
        assert_eq!(capped.backoff(25).unwrap().as_millis(), 3000, "shift clamp");
    }

    #[test]
    fn control_plane_schedule() {
        // The default profile (another test may have installed a [remote]
        // override into the process-global holder).
        let p = ReconnectPolicy::control_plane_default();
        assert_eq!(p.backoff(1).unwrap().as_millis(), 1000);
        assert_eq!(p.backoff(2).unwrap().as_millis(), 2000);
        assert_eq!(p.backoff(3).unwrap().as_millis(), 4000);
        assert_eq!(p.backoff(4).unwrap().as_millis(), 8000);
        assert_eq!(p.backoff(5), None, "4 attempts max");
    }

    #[test]
    fn probe_schedule() {
        let p = ReconnectPolicy::probe();
        assert_eq!(p.backoff(1).unwrap().as_millis(), 1000);
        assert_eq!(p.backoff(2).unwrap().as_millis(), 2000);
        assert_eq!(p.backoff(3).unwrap().as_millis(), 4000);
        assert_eq!(p.backoff(4), None);
    }

    #[test]
    fn should_reconnect_only_on_transport_drop_within_budget() {
        let p = ReconnectPolicy::default();
        assert!(p.should_reconnect(255, 1));
        assert!(p.should_reconnect(255, 5));
        assert!(!p.should_reconnect(255, 6), "budget exhausted");
        assert!(!p.should_reconnect(0, 1), "clean exit is terminal");
        assert!(!p.should_reconnect(1, 1), "app exit is terminal");
    }

    #[test]
    fn should_retry_truth_table() {
        let p = ReconnectPolicy::control_plane_default(); // 4 total attempts
        assert!(p.should_retry(ErrorClass::Transient, 1));
        assert!(p.should_retry(ErrorClass::Transient, 3));
        assert!(
            !p.should_retry(ErrorClass::Transient, 4),
            "4th was the last"
        );
        assert!(!p.should_retry(ErrorClass::Permanent, 1), "permanent");
    }

    fn zero_budget(max_attempts: u32) -> StepBudget {
        StepBudget::new(
            ReconnectPolicy {
                max_attempts,
                base_delay_ms: 0,
                max_delay_ms: 0,
            },
            Duration::from_secs(60),
        )
    }

    #[test]
    fn with_retry_transient_then_ok() {
        let mut calls = 0;
        let mut notes = Vec::new();
        let mut betweens = 0;
        let got = with_retry(
            "connect",
            &zero_budget(4),
            &mut |n| notes.push(n),
            &mut || betweens += 1,
            &mut || {
                calls += 1;
                if calls < 3 {
                    Err(ClassifiedErr::transient("flap"))
                } else {
                    Ok(42)
                }
            },
        );
        assert_eq!(got.unwrap(), 42);
        assert_eq!(calls, 3, "two flaps ride through");
        assert_eq!(betweens, 2, "hygiene hook before each retry");
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("retrying connect (1/4)"), "{}", notes[0]);
        assert!(notes[0].contains("flap"), "{}", notes[0]);
    }

    #[test]
    fn with_retry_permanent_short_circuits() {
        let mut calls = 0;
        let err = with_retry(
            "connect",
            &zero_budget(4),
            &mut |_| {},
            &mut || {},
            &mut || -> Result<(), _> {
                calls += 1;
                Err(ClassifiedErr::permanent("denied"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1, "no retry on permanent");
        assert_eq!(err.msg, "denied", "single attempt ⇒ no annotation");
    }

    #[test]
    fn with_retry_exhausts_attempts_and_annotates() {
        let mut calls = 0;
        let err = with_retry(
            "probe",
            &zero_budget(3),
            &mut |_| {},
            &mut || {},
            &mut || -> Result<(), _> {
                calls += 1;
                Err(ClassifiedErr::transient("drop"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 3, "max_attempts = total tries");
        assert!(err.is_transient(), "class survives exhaustion");
        assert!(err.msg.contains("after 3 attempts"), "{}", err.msg);
    }

    #[test]
    fn with_retry_respects_overall_wall_clock() {
        // 100ms backoff but a 1ms overall budget: the first retry would blow
        // the budget, so we give up after one attempt.
        let budget = StepBudget::new(
            ReconnectPolicy {
                max_attempts: 10,
                base_delay_ms: 100,
                max_delay_ms: 100,
            },
            Duration::from_millis(1),
        );
        let mut calls = 0;
        let err = with_retry(
            "pull",
            &budget,
            &mut |_| {},
            &mut || {},
            &mut || -> Result<(), _> {
                calls += 1;
                Err(ClassifiedErr::transient("drop"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1, "overall cap stops a slow-fail loop");
        assert!(err.is_transient());
    }
}
