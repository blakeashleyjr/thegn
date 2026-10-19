//! Pure thermal plausibility, alias, classification, and alarm policy.

/// T26: plausible thermal input range, in degrees Celsius.
pub const MIN_TEMP_C: f32 = -40.0;
pub const MAX_TEMP_C: f32 = 150.0;
/// T26: fixed-value stuck window.
pub const STUCK_AFTER_MS: u64 = 60_000;
/// T26: alias tolerance and consecutive-sample requirement.
pub const ALIAS_TOLERANCE_C: f32 = 0.2;
pub const ALIAS_SAMPLES: u8 = 3;
/// T5/T20: alarm re-cross hysteresis.
pub const ALARM_HYSTERESIS_C: f32 = 2.0;
/// Minimum paired readings used for the T25 display hint.
pub const PAIR_MIN_SAMPLES: usize = 30;
/// Minimum Pearson correlation for suggesting a fan/sensor pair.
pub const PAIR_MIN_R: f64 = 0.5;

/// Returns the highest-correlated temperature sensor using newest-aligned
/// finite samples. Ring alignment is approximate when one series skips gaps.
pub fn pair_fan(fan: &[f32], sensors: &[&[f32]], min_samples: usize) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (sensor_index, sensor) in sensors.iter().enumerate() {
        let aligned = fan.len().min(sensor.len());
        let fan_start = fan.len().saturating_sub(aligned);
        let sensor_start = sensor.len().saturating_sub(aligned);
        let mut count = 0_usize;
        let mut sum_x = 0.0_f64;
        let mut sum_y = 0.0_f64;
        let mut sum_xx = 0.0_f64;
        let mut sum_yy = 0.0_f64;
        let mut sum_xy = 0.0_f64;
        for (&x, &y) in fan[fan_start..].iter().zip(&sensor[sensor_start..]) {
            if !x.is_finite() || !y.is_finite() {
                continue;
            }
            let x = f64::from(x);
            let y = f64::from(y);
            count += 1;
            sum_x += x;
            sum_y += y;
            sum_xx += x * x;
            sum_yy += y * y;
            sum_xy += x * y;
        }
        if count < min_samples {
            continue;
        }
        let n = count as f64;
        let var_x = n * sum_xx - sum_x * sum_x;
        let var_y = n * sum_yy - sum_y * sum_y;
        if var_x <= 0.0 || var_y <= 0.0 {
            continue;
        }
        let correlation = (n * sum_xy - sum_x * sum_y) / (var_x * var_y).sqrt();
        if correlation >= PAIR_MIN_R
            && best.is_none_or(|(_, best_correlation)| correlation > best_correlation)
        {
            best = Some((sensor_index, correlation));
        }
    }
    best.map(|(index, _)| index)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Plausibility {
    previous: Option<f32>,
    unchanged_since_ms: u64,
}

impl Plausibility {
    pub fn accept(&mut self, temp_c: f32, now_ms: u64) -> bool {
        if !plausible_range(temp_c) {
            return false;
        }
        if self.previous == Some(temp_c) {
            // 255 is outside plausible_range today; keep it for T26 parity if that range changes.
            let sentinel = temp_c == 0.0 || temp_c == 127.0 || temp_c == 255.0;
            if now_ms.saturating_sub(self.unchanged_since_ms) >= STUCK_AFTER_MS && sentinel {
                return false;
            }
        } else {
            self.previous = Some(temp_c);
            self.unchanged_since_ms = now_ms;
        }
        true
    }
}

pub fn plausible_range(temp_c: f32) -> bool {
    temp_c.is_finite() && (MIN_TEMP_C..=MAX_TEMP_C).contains(&temp_c)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasDetector {
    runs: Vec<u8>,
    aliases: Vec<bool>,
}

impl AliasDetector {
    /// Compares zones to temperature hwmon sensors in layout order. A zone is
    /// an alias when one labelled sensor matches for three consecutive calls.
    pub fn update(
        &mut self,
        zones: &[Option<f32>],
        hwmon_values: &[Option<f32>],
        hwmon_labelled: &[bool],
    ) -> &[bool] {
        self.runs.resize(zones.len(), 0);
        self.aliases.resize(zones.len(), false);
        for (index, zone_temp) in zones.iter().enumerate() {
            if self.aliases[index] {
                continue;
            }
            let matched = zone_temp.is_some_and(|zone| {
                hwmon_values
                    .iter()
                    .zip(hwmon_labelled)
                    .any(|(value, labelled)| {
                        *labelled
                            && value.is_some_and(|value| (zone - value).abs() <= ALIAS_TOLERANCE_C)
                    })
            });
            self.runs[index] = if matched {
                self.runs[index].saturating_add(1)
            } else {
                0
            };
            if self.runs[index] >= ALIAS_SAMPLES {
                self.aliases[index] = true;
            }
        }
        &self.aliases
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AlarmDetector {
    active: bool,
    events: u64,
}

impl AlarmDetector {
    /// Counts one event for a crossing; a re-cross within 2 C remains the same
    /// event until the reading falls farther below the threshold.
    pub fn update(&mut self, temp_c: f32, threshold_c: f32) -> u64 {
        if temp_c >= threshold_c {
            if !self.active {
                self.events = self.events.saturating_add(1);
                self.active = true;
            }
        } else if temp_c < threshold_c - ALARM_HYSTERESIS_C {
            self.active = false;
        }
        self.events
    }
    pub fn events(&self) -> u64 {
        self.events
    }
}

pub fn classify_trip(kind: &str) -> crate::collect::model::thermal_zone::TripKind {
    use crate::collect::model::thermal_zone::TripKind;
    match kind.trim() {
        "passive" => TripKind::Passive,
        "active" => TripKind::Active,
        "hot" => TripKind::Hot,
        "critical" => TripKind::Critical,
        _ => TripKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plausibility_rejects_out_of_range_and_stuck_sentinels_after_60_seconds() {
        let mut state = Plausibility::default();
        assert!(!state.accept(-41.0, 0));
        assert!(!state.accept(151.0, 0));
        assert!(state.accept(127.0, 0));
        assert!(state.accept(127.0, 59_999));
        assert!(!state.accept(127.0, 60_001));
    }

    #[test]
    fn alias_requires_three_consecutive_matches_and_gap_resets_run() {
        let hwmon_values = [Some(50.0)];
        let hwmon_labelled = [true];
        let mut detector = AliasDetector::default();
        assert_eq!(
            detector.update(&[Some(50.1)], &hwmon_values, &hwmon_labelled),
            [false]
        );
        assert_eq!(
            detector.update(&[Some(50.1)], &hwmon_values, &hwmon_labelled),
            [false]
        );
        assert_eq!(
            detector.update(&[Some(50.3)], &hwmon_values, &hwmon_labelled),
            [false]
        );
        assert_eq!(
            detector.update(&[Some(50.1)], &hwmon_values, &hwmon_labelled),
            [false]
        );
        assert_eq!(
            detector.update(&[Some(50.1)], &hwmon_values, &hwmon_labelled),
            [false]
        );
        assert_eq!(
            detector.update(&[Some(50.1)], &hwmon_values, &hwmon_labelled),
            [true]
        );
    }

    #[test]
    fn alarm_rerecross_within_two_degrees_is_one_event() {
        let mut alarm = AlarmDetector::default();
        assert_eq!(alarm.update(81.0, 80.0), 1);
        assert_eq!(alarm.update(79.0, 80.0), 1);
        assert_eq!(alarm.update(81.0, 80.0), 1);
        assert_eq!(alarm.update(77.9, 80.0), 1);
        assert_eq!(alarm.update(81.0, 80.0), 2);
    }

    #[test]
    fn fan_pairing_picks_highest_positive_correlation_and_lowest_tie() {
        let fan = (0..40).map(|value| value as f32).collect::<Vec<_>>();
        let exact = (0..40).map(|value| value as f32 * 2.0).collect::<Vec<_>>();
        let tied = exact.clone();
        let noisy = (0..40)
            .map(|value| (value as f32 * 1.7) + (value % 3) as f32)
            .collect::<Vec<_>>();
        assert_eq!(pair_fan(&fan, &[&noisy, &exact, &tied], 30), Some(1));
        assert_eq!(pair_fan(&fan, &[&exact, &tied], 30), Some(0));
    }

    #[test]
    fn fan_pairing_rejects_anti_correlated_flat_short_and_skips_nan() {
        let fan = (0..40).map(|value| value as f32).collect::<Vec<_>>();
        let anti = (0..40).map(|value| -(value as f32)).collect::<Vec<_>>();
        let flat = vec![4.0; 40];
        assert_eq!(pair_fan(&fan, &[&anti, &flat], 30), None);
        assert_eq!(pair_fan(&fan[..29], &[&fan[..29]], 30), None);

        let mut with_gaps = fan.clone();
        with_gaps[3] = f32::NAN;
        with_gaps[11] = f32::NAN;
        assert_eq!(pair_fan(&fan, &[&with_gaps], 38), Some(0));
        assert_eq!(pair_fan(&fan, &[&with_gaps], 39), None);
    }
}
