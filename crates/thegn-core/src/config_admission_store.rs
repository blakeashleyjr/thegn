//! The live store of admitted configuration generations.
//!
//! One `Mutex` guards the whole state (current snapshot, next generation,
//! failure epoch and health), so a reader can never observe a snapshot from
//! one publication paired with the health of another, and a failed reload can
//! never interleave with a publication.
//!
//! Semantics:
//!
//! - A generation is allocated only when a complete [`AdmittedConfig`] is
//!   published. The first publication is generation 1; generations are
//!   strictly increasing and exhaustion is a typed refusal, never a wrap.
//! - Publication is compare-and-swap on the generation the publisher observed
//!   when it began capturing, so a slow reload cannot overwrite a newer one.
//! - A failed reload keeps the prior snapshot for **display**
//!   ([`AdmissionStore::display`]) but marks the store degraded; while degraded,
//!   [`AdmissionStore::authorize`] refuses every revision, so last-good can
//!   never authorize a new authority-bearing effect. Repeated identical
//!   failures coalesce into one diagnostic (the failure epoch counts them).
//! - `authorize` is a check at a publication boundary, not a lock: it proves
//!   that the revision an operation observed is still the current healthy one
//!   at the instant of the call. Callers re-check immediately before each
//!   authority side effect (provider request, sandbox launch, agent spawn,
//!   credential export, pipeline claim). An in-flight remote request is not
//!   made cancellable by a later reload.
//!
//! This is the single generation token downstream consumers bind to: the
//! [`ConfigRevision`] carries the generation plus the admitted digest; there
//! is deliberately no second digest or version counter.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config_admission::{AdmittedConfig, ConfigAdmissionError, ConfigRevision};

/// Why an operation's observed revision may not authorize a new effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleConfigReason {
    /// Nothing has been published yet.
    NoSnapshot,
    /// A newer generation has been published since the revision was observed.
    Superseded,
    /// The latest reload failed; the retained snapshot is display-only.
    Degraded,
    /// The generation counter cannot advance further.
    GenerationExhausted,
    /// The current generation was admitted without the state store's host
    /// definitions; it is display/recovery-only.
    HostsUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleConfigError {
    pub reason: StaleConfigReason,
    /// The generation that is current (0 when nothing is published).
    pub current_generation: u64,
}

impl fmt::Display for StaleConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            StaleConfigReason::NoSnapshot => "no admitted configuration is published",
            StaleConfigReason::Superseded => "the observed configuration generation was superseded",
            StaleConfigReason::Degraded => {
                "the latest configuration reload failed; the retained generation is display-only"
            }
            StaleConfigReason::GenerationExhausted => {
                "the configuration generation counter is exhausted"
            }
            StaleConfigReason::HostsUnavailable => {
                "the current configuration generation has no admitted host definitions"
            }
        };
        write!(
            f,
            "{reason} (current generation {})",
            self.current_generation
        )
    }
}

impl std::error::Error for StaleConfigError {}

/// Observable store health, shared by status/doctor/UI consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreHealth {
    Empty,
    Current {
        generation: u64,
    },
    /// The latest reload failed with `error`; `failures` counts consecutive
    /// failed attempts since the last successful publication.
    Degraded {
        generation: u64,
        error: ConfigAdmissionError,
        failures: u64,
    },
}

/// Whether a recorded failure is new information worth one diagnostic, or a
/// repeat of the failure already reported for this failure epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReport {
    First,
    Coalesced,
}

struct State {
    current: Option<Arc<AdmittedConfig>>,
    /// (category, detail fingerprint, consecutive failures)
    degraded: Option<(ConfigAdmissionError, u64, u64)>,
}

#[derive(Default)]
pub struct AdmissionStore {
    state: Mutex<Option<State>>,
}

impl AdmissionStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Option<State>> {
        // A panic while holding this lock cannot leave a half-written state:
        // every mutation below replaces whole fields after all checks pass.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn current_generation(state: &Option<State>) -> u64 {
        state
            .as_ref()
            .and_then(|state| state.current.as_ref())
            .map_or(0, |current| current.revision().generation)
    }

    /// Publish a complete candidate if the current generation still equals
    /// `expected_generation` (0 for the first publication). On success the
    /// store is healthy again and the returned snapshot carries its new
    /// generation.
    pub fn publish(
        &self,
        candidate: AdmittedConfig,
        expected_generation: u64,
    ) -> Result<Arc<AdmittedConfig>, StaleConfigError> {
        let mut guard = self.lock();
        let current = Self::current_generation(&guard);
        if current != expected_generation {
            return Err(StaleConfigError {
                reason: StaleConfigReason::Superseded,
                current_generation: current,
            });
        }
        let Some(next) = current.checked_add(1) else {
            return Err(StaleConfigError {
                reason: StaleConfigReason::GenerationExhausted,
                current_generation: current,
            });
        };
        let published = Arc::new(candidate.with_generation(next));
        *guard = Some(State {
            current: Some(Arc::clone(&published)),
            degraded: None,
        });
        Ok(published)
    }

    /// Record a failed reload. The prior snapshot stays for display only.
    /// `detail` is a fingerprint of the bounded diagnostic (e.g. a hash of
    /// its text): a failure is coalesced only when both its category and its
    /// detail repeat, so a *different* problem of the same category is new
    /// information and reported again.
    pub fn record_failure(&self, error: ConfigAdmissionError, detail: u64) -> FailureReport {
        let mut guard = self.lock();
        let state = guard.get_or_insert(State {
            current: None,
            degraded: None,
        });
        match &mut state.degraded {
            Some((previous, previous_detail, failures))
                if *previous == error && *previous_detail == detail =>
            {
                *failures = failures.saturating_add(1);
                FailureReport::Coalesced
            }
            Some((previous, previous_detail, failures)) => {
                *previous = error;
                *previous_detail = detail;
                *failures = failures.saturating_add(1);
                FailureReport::First
            }
            degraded => {
                *degraded = Some((error, detail, 1));
                FailureReport::First
            }
        }
    }

    /// The latest published snapshot, degraded or not. For display, status,
    /// and teardown that already holds its own authority — never for a new
    /// authority-bearing operation (use [`Self::authorize`]).
    pub fn display(&self) -> Option<Arc<AdmittedConfig>> {
        self.lock()
            .as_ref()
            .and_then(|state| state.current.as_ref().map(Arc::clone))
    }

    /// The current snapshot only while the store is healthy.
    pub fn current(&self) -> Result<Arc<AdmittedConfig>, StaleConfigError> {
        let guard = self.lock();
        let generation = Self::current_generation(&guard);
        let Some(state) = guard.as_ref() else {
            return Err(StaleConfigError {
                reason: StaleConfigReason::NoSnapshot,
                current_generation: 0,
            });
        };
        if state.degraded.is_some() {
            return Err(StaleConfigError {
                reason: StaleConfigReason::Degraded,
                current_generation: generation,
            });
        }
        let current = state
            .current
            .as_ref()
            .map(Arc::clone)
            .ok_or(StaleConfigError {
                reason: StaleConfigReason::NoSnapshot,
                current_generation: 0,
            })?;
        if current.health() == crate::config_admission::AdmissionHealth::HostsUnavailable {
            return Err(StaleConfigError {
                reason: StaleConfigReason::HostsUnavailable,
                current_generation: generation,
            });
        }
        Ok(current)
    }

    /// Prove that `observed` is still the current, healthy revision.
    pub fn authorize(
        &self,
        observed: &ConfigRevision,
    ) -> Result<Arc<AdmittedConfig>, StaleConfigError> {
        let current = self.current()?;
        if current.revision() != *observed {
            return Err(StaleConfigError {
                reason: StaleConfigReason::Superseded,
                current_generation: current.revision().generation,
            });
        }
        Ok(current)
    }

    pub fn health(&self) -> StoreHealth {
        let guard = self.lock();
        let generation = Self::current_generation(&guard);
        match guard.as_ref() {
            None => StoreHealth::Empty,
            Some(State {
                degraded: Some((error, _, failures)),
                ..
            }) => StoreHealth::Degraded {
                generation,
                error: error.clone(),
                failures: *failures,
            },
            Some(State { current: None, .. }) => StoreHealth::Empty,
            Some(_) => StoreHealth::Current { generation },
        }
    }
}

#[cfg(test)]
#[path = "config_admission_store_tests.rs"]
mod tests;
