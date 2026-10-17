//! Collector health state and bounded retry policy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    NotImplemented,
    NotOnThisOs,
    NoHardware,
    NoPermission,
}

impl Absent {
    pub fn to_coverage(self) -> crate::AbsentReason {
        match self {
            Self::NotImplemented | Self::NotOnThisOs => crate::AbsentReason::NotOnThisOs,
            Self::NoHardware => crate::AbsentReason::NoHardware,
            Self::NoPermission => crate::AbsentReason::NoPermission,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Health {
    Ok,
    #[default]
    Warming,
    Absent(Absent),
    Failing {
        since_tick: u64,
        backoff: u16,
        retry_at: u64,
    },
}

impl Health {
    pub fn after_error(self, tick: u64) -> Self {
        let (since_tick, previous) = match self {
            Self::Failing {
                since_tick,
                backoff,
                ..
            } => (since_tick, backoff),
            _ => (tick, 0),
        };
        let backoff = if previous == 0 {
            1
        } else {
            previous.saturating_mul(2).min(60)
        };
        Self::Failing {
            since_tick,
            backoff,
            retry_at: tick.saturating_add(u64::from(backoff)),
        }
    }
    pub fn after_ok(self) -> Self {
        Self::Ok
    }
    pub fn retry_due(&self, tick: u64) -> bool {
        match self {
            Self::Failing { retry_at, .. } => tick >= *retry_at,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Absent, Health};
    use crate::AbsentReason;

    #[test]
    fn failure_backoff_is_bounded_and_keeps_initial_tick() {
        let mut health = Health::Ok;
        let expected = [1, 2, 4, 8, 16, 32, 60, 60];
        for (tick, backoff) in expected.into_iter().enumerate() {
            health = health.after_error(tick as u64);
            assert!(
                matches!(health, Health::Failing { since_tick: 0, backoff: actual, .. } if actual == backoff)
            );
        }
        assert!(!health.retry_due(66));
        assert!(health.retry_due(67));
        assert_eq!(health.after_ok(), Health::Ok);
    }

    #[test]
    fn absent_reasons_map_to_coverage_vocabulary() {
        assert_eq!(
            Absent::NotImplemented.to_coverage(),
            AbsentReason::NotOnThisOs
        );
        assert_eq!(Absent::NotOnThisOs.to_coverage(), AbsentReason::NotOnThisOs);
        assert_eq!(Absent::NoHardware.to_coverage(), AbsentReason::NoHardware);
        assert_eq!(
            Absent::NoPermission.to_coverage(),
            AbsentReason::NoPermission
        );
    }
}
