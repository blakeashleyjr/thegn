//! Pure cadence and demand scheduling.

use super::{COLLECTORS, CollectorId, CollectorSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    Every(u16),
    Static,
}
impl Cadence {
    pub const fn period(self) -> u16 {
        match self {
            Self::Every(0) => 1,
            Self::Every(n) => n,
            Self::Static => 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open,
    Paused,
    Hidden,
    Closed,
}

/// Spread slow readers across their period slots in declaration order.
pub fn phases(collectors: &[(CollectorId, Cadence)]) -> [u16; COLLECTORS] {
    let mut result = [0; COLLECTORS];
    let mut next = 0_u16;
    for (id, cadence) in collectors {
        let period = cadence.period();
        if period > 1 {
            result[id.index()] = next % period;
            next = next.wrapping_add(1);
        }
    }
    result
}

pub fn due(
    mode: Mode,
    demand: CollectorSet,
    id: CollectorId,
    cadence: Cadence,
    phase: u16,
    tick: u64,
) -> bool {
    if mode != Mode::Open || !demand.contains(id) {
        return false;
    }
    let period = u64::from(cadence.period());
    tick % period == u64::from(phase) % period
}

pub fn due_set(
    mode: Mode,
    demand: CollectorSet,
    cadences: &[(CollectorId, Cadence)],
    phases: &[u16; COLLECTORS],
    tick: u64,
) -> CollectorSet {
    let mut result = CollectorSet::empty();
    for (id, cadence) in cadences {
        if due(mode, demand, *id, *cadence, phases[id.index()], tick) {
            result.insert(*id);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Cadence, Mode, due, due_set, phases};
    use crate::{CollectorId, CollectorSet};

    #[test]
    fn cadence_and_demand_are_enforced() {
        let mut demand = CollectorSet::empty();
        demand.insert(CollectorId::Loadavg);
        assert!(due(
            Mode::Open,
            demand,
            CollectorId::Loadavg,
            Cadence::Every(1),
            99,
            99
        ));
        assert_eq!(
            (0..100)
                .filter(|tick| due(
                    Mode::Open,
                    demand,
                    CollectorId::Loadavg,
                    Cadence::Every(5),
                    0,
                    *tick
                ))
                .count(),
            20
        );
        assert_eq!(Cadence::Static.period(), 30);
        assert!(!due(
            Mode::Open,
            CollectorSet::empty(),
            CollectorId::Loadavg,
            Cadence::Every(1),
            0,
            0
        ));
    }

    #[test]
    fn phases_spread_slow_collectors_and_non_open_modes_park() {
        let ids = [
            CollectorId::Loadavg,
            CollectorId::Uptime,
            CollectorId::Psi,
            CollectorId::CpuStat,
        ];
        let inputs = ids.map(|id| (id, Cadence::Every(5)));
        let phase = phases(&inputs);
        for left in 0..ids.len() {
            for right in left + 1..ids.len() {
                assert_ne!(phase[ids[left].index()], phase[ids[right].index()]);
            }
        }
        let mut demand = CollectorSet::empty();
        for id in ids {
            demand.insert(id);
        }
        for mode in [Mode::Paused, Mode::Hidden, Mode::Closed] {
            assert!(due_set(mode, demand, &inputs, &phase, 3).is_empty());
        }
    }
}
