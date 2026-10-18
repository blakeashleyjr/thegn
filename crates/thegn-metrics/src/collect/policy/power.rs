//! Pure battery-state and energy policy over T-2 power-supply rows.
use super::super::model::power_supply::SupplyStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveState {
    Charging,
    Discharging,
    Held,
    Full,
    IdleOnAc,
}

pub fn effective_state(
    status: Option<SupplyStatus>,
    ac_online: Option<bool>,
    capacity_pct: Option<u8>,
    end_threshold_pct: Option<u8>,
) -> Option<EffectiveState> {
    match status {
        Some(SupplyStatus::Charging) => Some(EffectiveState::Charging),
        Some(SupplyStatus::Discharging) => Some(EffectiveState::Discharging),
        Some(SupplyStatus::Full) => Some(EffectiveState::Full),
        Some(SupplyStatus::NotCharging) => match ac_online {
            Some(false) => Some(EffectiveState::Discharging),
            Some(true)
                if capacity_pct
                    .zip(end_threshold_pct)
                    .is_some_and(|(capacity, threshold)| capacity >= threshold) =>
            {
                Some(EffectiveState::Held)
            }
            Some(true) => Some(EffectiveState::IdleOnAc),
            None => None,
        },
        Some(SupplyStatus::Unknown) | None => match ac_online {
            Some(false) => Some(EffectiveState::Discharging),
            Some(true) => Some(EffectiveState::IdleOnAc),
            None => None,
        },
    }
}

/// Apply the state-derived battery sign: discharge is positive, charge negative.
pub fn signed_battery_watts(magnitude_watts: f32, state: EffectiveState) -> f32 {
    match state {
        EffectiveState::Discharging => magnitude_watts.abs(),
        EffectiveState::Charging => -magnitude_watts.abs(),
        EffectiveState::Held | EffectiveState::Full | EffectiveState::IdleOnAc => 0.0,
    }
}

/// Return hours remaining, using current energy for discharge and headroom for charge.
pub fn eta(
    energy_now_wh: f64,
    energy_full_wh: f64,
    ewma_watts: f32,
    state: EffectiveState,
) -> Option<f64> {
    if !ewma_watts.is_finite()
        || ewma_watts.abs() < 0.5
        || !energy_now_wh.is_finite()
        || !energy_full_wh.is_finite()
    {
        return None;
    }
    let remaining_wh = match state {
        EffectiveState::Discharging => energy_now_wh,
        EffectiveState::Charging => energy_full_wh - energy_now_wh,
        EffectiveState::Held | EffectiveState::Full | EffectiveState::IdleOnAc => return None,
    };
    (remaining_wh >= 0.0).then_some(remaining_wh / f64::from(ewma_watts.abs()))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ewma {
    watts: Option<f32>,
    at_ms: Option<u64>,
}

impl Ewma {
    pub const fn watts(self) -> Option<f32> {
        self.watts
    }

    pub fn update(&mut self, watts: f32, now_ms: u64) -> Option<f32> {
        if !watts.is_finite() {
            return self.watts;
        }
        match (self.watts, self.at_ms) {
            (Some(previous), Some(previous_ms)) => {
                let dt_ms = now_ms.saturating_sub(previous_ms);
                if dt_ms > 0 {
                    let alpha = 1.0 - (-(dt_ms as f64 / 60_000.0)).exp();
                    self.watts =
                        Some((f64::from(previous) + alpha * f64::from(watts - previous)) as f32);
                }
            }
            _ => self.watts = Some(watts),
        }
        self.at_ms = Some(now_ms);
        self.watts
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionEnergy {
    energy_wh: f64,
    previous: Option<(u64, f32)>,
    previous_ac_online: Option<bool>,
}

impl SessionEnergy {
    pub const fn energy_wh(self) -> f64 {
        self.energy_wh
    }

    /// Integrate positive discharge watts. Gaps over 30 seconds add no energy.
    pub fn update(&mut self, now_ms: u64, ac_online: Option<bool>, watts: Option<f32>) -> f64 {
        if self.previous_ac_online == Some(true) && ac_online == Some(false) {
            self.energy_wh = 0.0;
            self.previous = None;
        }
        if let (Some((previous_ms, previous_watts)), Some(watts)) = (self.previous, watts) {
            let dt_ms = now_ms.saturating_sub(previous_ms);
            if dt_ms > 0 && dt_ms <= 30_000 {
                let average_watts = (previous_watts.max(0.0) + watts.max(0.0)) / 2.0;
                self.energy_wh += f64::from(average_watts) * dt_ms as f64 / 3_600_000.0;
            }
        }
        self.previous = watts.map(|watts| (now_ms, watts));
        self.previous_ac_online = ac_online;
        self.energy_wh
    }
}

#[cfg(test)]
mod tests {
    use super::{EffectiveState, Ewma, SessionEnergy, effective_state, eta, signed_battery_watts};
    use crate::collect::model::power_supply::SupplyStatus;

    #[test]
    fn effective_state_distinguishes_charge_hold_full_and_discharge() {
        let cases = [
            (
                Some(SupplyStatus::Charging),
                Some(true),
                None,
                None,
                Some(EffectiveState::Charging),
            ),
            (
                Some(SupplyStatus::Discharging),
                Some(true),
                Some(80),
                Some(80),
                Some(EffectiveState::Discharging),
            ),
            (
                Some(SupplyStatus::NotCharging),
                Some(true),
                Some(80),
                Some(80),
                Some(EffectiveState::Held),
            ),
            (
                Some(SupplyStatus::NotCharging),
                Some(true),
                Some(79),
                Some(80),
                Some(EffectiveState::IdleOnAc),
            ),
            (
                Some(SupplyStatus::NotCharging),
                Some(false),
                None,
                None,
                Some(EffectiveState::Discharging),
            ),
            (
                Some(SupplyStatus::Full),
                Some(true),
                Some(100),
                Some(80),
                Some(EffectiveState::Full),
            ),
            (None, None, None, None, None),
        ];
        for (status, ac, capacity, threshold, expected) in cases {
            assert_eq!(effective_state(status, ac, capacity, threshold), expected);
        }
        assert!((signed_battery_watts(12.0, EffectiveState::Discharging) - 12.0).abs() < 0.001);
        assert!((signed_battery_watts(12.0, EffectiveState::Charging) + 12.0).abs() < 0.001);
    }

    #[test]
    fn eta_has_a_half_watt_floor_and_requires_active_charge_state() {
        assert_eq!(eta(20.0, 40.0, 0.49, EffectiveState::Discharging), None);
        assert_eq!(eta(20.0, 40.0, 4.0, EffectiveState::Held), None);
        assert_eq!(eta(20.0, 40.0, 4.0, EffectiveState::Discharging), Some(5.0));
        assert_eq!(eta(20.0, 40.0, -4.0, EffectiveState::Charging), Some(5.0));
    }

    #[test]
    fn ewma_uses_elapsed_time_and_session_energy_resets_and_skips_large_gaps() {
        let mut ewma = Ewma::default();
        assert!((ewma.update(10.0, 0).unwrap() - 10.0).abs() < 0.001);
        let after_30s = ewma.update(20.0, 30_000).unwrap();
        let expected = 10.0 + (1.0 - (-0.5_f64).exp()) * 10.0;
        assert!((f64::from(after_30s) - expected).abs() < 0.001);
        let after_90s = ewma.update(0.0, 90_000).unwrap();
        let expected_after_90s = f64::from(after_30s) * (-1.0_f64).exp();
        assert!((f64::from(after_90s) - expected_after_90s).abs() < 0.001);

        let mut session = SessionEnergy::default();
        session.update(0, Some(true), Some(10.0));
        session.update(10_000, Some(true), Some(10.0));
        assert!((session.energy_wh() - 10.0 / 360.0).abs() < 0.0001);
        session.update(11_000, Some(false), Some(10.0));
        assert_eq!(session.energy_wh(), 0.0);
        session.update(51_000, Some(false), Some(10.0));
        assert_eq!(session.energy_wh(), 0.0);
    }
}
