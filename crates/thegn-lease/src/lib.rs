//! Pure transitions for one run attempt. The queue owns the authoritative clock,
//! selects the shard's current row and persists each mutation in its transaction.
//! Keep completed attempt rows for idempotent receipt lookup after reassignment.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Token {
    pub run_id: String,
    /// Authenticated worker identity, including its owning trust domain.
    pub worker_id: String,
    pub epoch: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Leased,
    Preparing,
    Running,
    Committing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interruption {
    Cancelled,
    Preempted,
    TimedOut,
    LeaseExpired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Active(Phase),
    Closed(Interruption),
    Committed { fingerprint: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttemptLease {
    token: Token,
    expires_at_ms: u64,
    deadline_ms: u64,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidClaim,
    EpochExhausted,
    Fenced,
    Expired,
    WrongPhase,
    ConflictingCommit,
    InvalidFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    Publish,
    Replay,
}

impl AttemptLease {
    /// Only call after winning the queued shard in the storage transaction.
    /// The store enforces uniqueness of run_id and persists the returned epoch.
    pub fn claim(
        previous_epoch: u64,
        run_id: String,
        worker_id: String,
        now_ms: u64,
        ttl_ms: NonZeroU64,
        deadline_ms: u64,
    ) -> Result<Self, Error> {
        if run_id.is_empty() || worker_id.is_empty() || deadline_ms <= now_ms {
            return Err(Error::InvalidClaim);
        }
        let epoch = previous_epoch
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(Error::EpochExhausted)?;
        Ok(Self {
            token: Token {
                run_id,
                worker_id,
                epoch,
            },
            expires_at_ms: now_ms.saturating_add(ttl_ms.get()).min(deadline_ms),
            deadline_ms,
            state: State::Active(Phase::Leased),
        })
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    fn active(&self, token: &Token, now_ms: u64) -> Result<Phase, Error> {
        if token != &self.token {
            return Err(Error::Fenced);
        }
        let State::Active(phase) = self.state else {
            return Err(Error::Fenced);
        };
        if now_ms >= self.expires_at_ms {
            return Err(Error::Expired);
        }
        Ok(phase)
    }

    pub fn renew(&mut self, token: &Token, now_ms: u64, ttl_ms: NonZeroU64) -> Result<(), Error> {
        self.active(token, now_ms)?;
        let renewal = now_ms.saturating_add(ttl_ms.get()).min(self.deadline_ms);
        self.expires_at_ms = self.expires_at_ms.max(renewal);
        Ok(())
    }

    /// Repeating the current phase is harmless when its reply was lost.
    pub fn advance(&mut self, token: &Token, now_ms: u64, to: Phase) -> Result<(), Error> {
        let from = self.active(token, now_ms)?;
        if from == to {
            return Ok(());
        }
        if !matches!(
            (from, to),
            (Phase::Leased, Phase::Preparing)
                | (Phase::Preparing, Phase::Running)
                | (Phase::Running, Phase::Committing)
        ) {
            return Err(Error::WrongPhase);
        }
        self.state = State::Active(to);
        Ok(())
    }

    /// The caller supplies a fingerprint of the entire validated publication.
    /// Only Publish authorizes inserting visible references and usage events.
    pub fn commit(
        &mut self,
        token: &Token,
        now_ms: u64,
        fingerprint: &str,
    ) -> Result<Commit, Error> {
        if token != &self.token {
            return Err(Error::Fenced);
        }
        if let State::Committed {
            fingerprint: accepted,
        } = &self.state
        {
            return if fingerprint == accepted {
                Ok(Commit::Replay)
            } else {
                Err(Error::ConflictingCommit)
            };
        }
        if self.active(token, now_ms)? != Phase::Committing {
            return Err(Error::WrongPhase);
        }
        if fingerprint.is_empty() {
            return Err(Error::InvalidFingerprint);
        }
        self.state = State::Committed {
            fingerprint: fingerprint.to_owned(),
        };
        Ok(Commit::Publish)
    }

    /// Authoritative cancel/preemption observations identify the specific attempt.
    /// The queue closes it before making a retry eligible.
    pub fn interrupt(
        &mut self,
        token: &Token,
        now_ms: u64,
        reason: Interruption,
    ) -> Result<(), Error> {
        if token != &self.token {
            return Err(Error::Fenced);
        }
        if self.state == State::Closed(reason) {
            return Ok(());
        }
        if !matches!(self.state, State::Active(_)) {
            return Err(Error::Fenced);
        }
        match reason {
            Interruption::LeaseExpired if now_ms < self.expires_at_ms => {
                return Err(Error::WrongPhase);
            }
            Interruption::TimedOut if now_ms < self.deadline_ms => return Err(Error::WrongPhase),
            _ => (),
        }
        self.state = State::Closed(reason);
        Ok(())
    }
}
