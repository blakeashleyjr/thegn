//! Battery state as `(percent 0–100, on AC)`. sysinfo does not cover battery,
//! so this is ours: a native sysfs reader on Linux (which can distinguish a
//! charge-capped-but-plugged-in battery from one that is truly discharging),
//! and `starship-battery` elsewhere.

use std::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct BatteryReading {
    pub battery: Option<(u8, bool)>,
    pub power_w: Option<f32>,
    pub eta_secs: Option<u64>,
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::BatteryReading;
    #[cfg(test)]
    use crate::collect::linux::power_supply::Counters;
    use crate::collect::linux::power_supply::{BaseSupply, SupplyReader};

    const REDISCOVER_EVERY: Duration = Duration::from_secs(60);
    pub(crate) struct BatteryReader {
        reader: SupplyReader,
        discovered_at: Option<Instant>,
        rows: Vec<BaseSupply>,
    }
    impl BatteryReader {
        pub(crate) fn new(base: PathBuf) -> Self {
            Self {
                reader: SupplyReader::at(base),
                discovered_at: None,
                rows: Vec::new(),
            }
        }
        pub(crate) fn sample(&mut self, now: Instant) -> BatteryReading {
            let due = self
                .discovered_at
                .is_none_or(|then| now.duration_since(then) >= REDISCOVER_EVERY);
            if due {
                self.reader.discover();
                self.discovered_at = Some(now);
            }
            match self.read_sample(true) {
                Ok(reading) => reading,
                Err(()) => {
                    if !due {
                        self.reader.discover();
                        self.discovered_at = Some(now);
                    }
                    self.read_sample(false).unwrap_or_default()
                }
            }
        }
        #[cfg(test)]
        pub(super) fn counters(&self) -> Counters {
            self.reader.counters()
        }
        #[cfg(test)]
        pub(super) fn fail_next_read_for_test(&mut self) {
            self.reader.fail_next_read_for_test();
        }
        #[cfg(test)]
        pub(super) fn fail_next_power_read_for_test(&mut self) {
            self.reader.fail_next_power_read_for_test();
        }
        fn read_sample(&mut self, signal_critical_errors: bool) -> Result<BatteryReading, ()> {
            self.reader
                .read_base(&mut self.rows, signal_critical_errors)
                .map_err(|_| ())?;
            let rows = &self.rows;
            let battery =
                combine_percent(rows).map(|pct| (pct, rows.first().is_some_and(|row| row.on_ac)));
            let mut watts = 0.0_f64;
            let mut has_power = false;
            let mut remaining = 0.0_f64;
            let mut rate = 0.0_f64;
            let mut has_eta = false;
            for row in rows {
                let battery_power = row.power_now.map(|v| v as f64).or_else(|| {
                    Some(
                        row.current_now?.unsigned_abs() as f64 * row.voltage_now? as f64
                            / 1_000_000.0,
                    )
                });
                if let Some(power) = battery_power.filter(|power| *power > 0.0) {
                    watts += power;
                    has_power = true;
                }
                if let Some(value) = row.energy_now {
                    let rem = if row.charging {
                        row.energy_full
                            .map(|full| full.saturating_sub(value) as f64)
                    } else {
                        Some(value as f64)
                    };
                    if let (Some(rem), Some(power)) = (rem, battery_power.filter(|p| *p > 0.0)) {
                        remaining += rem;
                        rate += power;
                        has_eta = true;
                    }
                } else if let Some(charge) = row.charge_now
                    && let Some(current) = row.current_now
                {
                    let target = if row.charging {
                        row.charge_full
                            .map(|full| full.saturating_sub(charge) as f64)
                    } else {
                        Some(charge as f64)
                    };
                    let scale = if rows.len() > 1 {
                        row.voltage_now.map(|voltage| voltage as f64 / 1_000_000.0)
                    } else {
                        Some(1.0)
                    };
                    if let Some(target) = target
                        && let Some(scale) = scale
                    {
                        remaining += target * scale;
                        rate += current.unsigned_abs() as f64 * scale;
                        has_eta = true;
                    }
                }
            }
            let power_w = has_power.then_some((watts / 1_000_000.0) as f32);
            let eta_secs = (has_eta && rate > 0.0).then_some((remaining / rate * 3600.0) as u64);
            Ok(BatteryReading {
                battery,
                power_w,
                eta_secs,
            })
        }
    }

    fn combine_percent(rows: &[BaseSupply]) -> Option<u8> {
        let first = rows.first()?;
        if rows.len() == 1 {
            return first.capacity.map(|value| value.min(100));
        }
        let capacities = rows
            .iter()
            .map(|row| row.capacity)
            .collect::<Option<Vec<_>>>();
        let energy_pairs = rows
            .iter()
            .map(|row| {
                if let (Some(now), Some(full)) = (row.energy_now, row.energy_full) {
                    Some((now as f64, full as f64))
                } else if let (Some(now), Some(full), Some(voltage)) =
                    (row.charge_now, row.charge_full, row.voltage_min_design)
                {
                    let scale = voltage as f64 / 1_000_000.0;
                    Some((now as f64 * scale, full as f64 * scale))
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>();
        if let Some(energy_pairs) = energy_pairs {
            let total_now: f64 = energy_pairs.iter().map(|(now, _)| now).sum();
            let total_full: f64 = energy_pairs.iter().map(|(_, full)| full).sum();
            if total_full > 0.0 {
                return Some((total_now / total_full * 100.0).round().clamp(0.0, 100.0) as u8);
            }
        }
        capacities.map(|values| {
            (values.iter().map(|value| u64::from(*value)).sum::<u64>() as f64 / values.len() as f64)
                .round()
                .clamp(0.0, 100.0) as u8
        })
    }
    #[cfg(test)]
    pub(super) fn parse_u64(bytes: &[u8]) -> Option<u64> {
        let text = match std::str::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return None,
        };
        text.trim().parse().ok()
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::BatteryReader;

#[cfg(not(target_os = "linux"))]
pub(crate) struct BatteryReader;

#[cfg(not(target_os = "linux"))]
impl BatteryReader {
    pub(crate) fn new(_base: std::path::PathBuf) -> Self {
        Self
    }
    pub(crate) fn sample(&mut self, _now: Instant) -> BatteryReading {
        let (power_w, eta_secs) = read_battery_power_starship();
        BatteryReading {
            battery: read_battery_starship(),
            power_w,
            eta_secs,
        }
    }
}

/// Read battery state: `(capacity %, on AC)`. `None` when there is no battery
/// (desktop / VM). The bool means "plugged in", not "actively charging".
#[cfg(target_os = "linux")]
pub fn read_battery(base: &std::path::Path) -> Option<(u8, bool)> {
    linux::BatteryReader::new(base.to_path_buf())
        .sample(Instant::now())
        .battery
}

#[cfg(not(target_os = "linux"))]
pub fn read_battery(_base: &std::path::Path) -> Option<(u8, bool)> {
    read_battery_starship()
}

/// Battery energy flow: `(power watts, seconds-to-full-or-empty)`. Both fields
/// are `Option` and independent — a tree may expose power without enough state
/// to project a time, or vice versa. Idle (power == 0) yields `(None, None)`.
#[cfg(target_os = "linux")]
pub fn read_battery_power(base: &std::path::Path) -> (Option<f32>, Option<u64>) {
    let reading = linux::BatteryReader::new(base.to_path_buf()).sample(Instant::now());
    (reading.power_w, reading.eta_secs)
}

#[cfg(not(target_os = "linux"))]
pub fn read_battery_power(_base: &std::path::Path) -> (Option<f32>, Option<u64>) {
    read_battery_power_starship()
}

/// Whether `state` means a charger is attached, given the host OS.
///
/// The Linux reader answers this from the *adapter's* `online` flag, so a
/// battery that is plugged in but not currently charging still reads correctly.
/// `starship-battery` exposes only the battery state, so the mapping has to be
/// inverted per platform — and getting it wrong is not hypothetical:
///
/// `Charging | Full` was the whole test, which made **a Mac with an 80% charge
/// limit, plugged in and idle, report "not on AC"** — precisely the bug the
/// Linux arm's adapter read was written to avoid, and an increasingly normal
/// configuration.
///
/// The darwin fix is exact rather than a guess, because the crate's own state
/// mapping is ordered (`platform/darwin/device.rs`):
///   `!external_connected` → `Discharging` (checked FIRST)
///   `is_charging`         → `Charging`
///   `capacity == 0`       → `Empty`
///   `fully_charged`       → `Full`
///   otherwise             → `Unknown`
/// so on macOS `Unknown` is reachable *only* with a charger attached — it is the
/// charge-capped state. Elsewhere `Unknown` genuinely means unknown, and
/// claiming AC would be inventing information, so it stays false there.
/// (Gated with its caller: `starship-battery` is only a dependency off Linux,
/// where the native sysfs reader answers this from the adapter directly.)
#[cfg(not(target_os = "linux"))]
pub(crate) fn state_means_on_ac(state: starship_battery::State, macos: bool) -> bool {
    use starship_battery::State;
    match state {
        State::Charging | State::Full => true,
        State::Discharging | State::Empty => false,
        State::Unknown => macos,
    }
}

/// Non-Linux battery via `starship-battery` (macOS IOKit, Windows
/// `GetSystemPowerStatus`, BSD). Reports the first battery; "on AC" per
/// [`state_means_on_ac`].
#[cfg(not(target_os = "linux"))]
fn read_battery_starship() -> Option<(u8, bool)> {
    use starship_battery::Manager;
    let manager = Manager::new().ok()?;
    let mut batteries = manager.batteries().ok()?;
    let bat = batteries.next()?.ok()?;
    // state_of_charge is a ratio 0.0..=1.0 (a `Ratio` quantity).
    let pct = (bat.state_of_charge().value * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    let on_ac = state_means_on_ac(bat.state(), cfg!(target_os = "macos"));
    Some((pct, on_ac))
}

/// Non-Linux power/ETA via `starship-battery`: `energy_rate` (watts) and the
/// crate's own `time_to_empty`/`time_to_full` projections.
#[cfg(not(target_os = "linux"))]
fn read_battery_power_starship() -> (Option<f32>, Option<u64>) {
    use starship_battery::{Manager, State};
    let Some(bat) = Manager::new()
        .ok()
        .and_then(|m| m.batteries().ok())
        .and_then(|mut b| b.next())
        .and_then(|b| b.ok())
    else {
        return (None, None);
    };
    let watts = {
        let w = bat.energy_rate().value;
        (w > 0.0).then_some(w)
    };
    let eta = match bat.state() {
        State::Discharging => bat.time_to_empty(),
        State::Charging => bat.time_to_full(),
        _ => None,
    }
    .map(|t| t.value as u64);
    (watts, eta)
}

// The `starship-battery` mapping is pure and worth pinning on the platforms
// that actually use it — the sysfs tests below are Linux-gated, which left every
// non-Linux arm of this module with no coverage at all.
#[cfg(all(test, not(target_os = "linux")))]
mod starship_tests {
    use super::*;
    use starship_battery::State;

    #[test]
    fn a_charge_capped_mac_still_reads_as_on_ac() {
        // The regression: plugged in, charge-limited to 80%, not charging. The
        // crate maps that to `Unknown` (its `!external_connected → Discharging`
        // arm is checked first, so `Unknown` implies a charger IS attached), and
        // the old `Charging | Full` test called it "on battery".
        assert!(state_means_on_ac(State::Unknown, true));
        // Off macOS that inference doesn't hold, so don't claim it.
        assert!(!state_means_on_ac(State::Unknown, false));

        // Unambiguous states agree on every platform.
        for macos in [true, false] {
            assert!(state_means_on_ac(State::Charging, macos));
            assert!(state_means_on_ac(State::Full, macos));
            assert!(!state_means_on_ac(State::Discharging, macos));
            assert!(!state_means_on_ac(State::Empty, macos));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn fixture(name: &str) -> tempfile::TempDir {
        let prefix = format!("tg-battery-{}-{name}-", std::process::id());
        let dir = tempfile::Builder::new().prefix(&prefix).tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn supply(base: &std::path::Path, name: &str, values: &[(&str, &str)]) {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        for (key, value) in values {
            std::fs::write(path.join(key), value).unwrap();
        }
    }

    #[test]
    fn read_battery_parses_fixture_tree() {
        let base = std::env::temp_dir().join(format!("tg-batt-{}", std::process::id()));
        let bat = base.join("BAT0");
        let ac = base.join("AC");
        let _ = std::fs::remove_dir_all(&base); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&bat).unwrap();
        std::fs::create_dir_all(&ac).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("capacity"), "73\n").unwrap();
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, true)));

        // Unplugged, no adapter `online` file: status alone drives it.
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, false)));

        // Charge-capped: battery reports Discharging while plugged in, but the
        // Mains adapter's online=1 still reads as on-AC (the bug fix).
        std::fs::write(ac.join("online"), "1\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, true)));
        std::fs::write(ac.join("online"), "0\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, false)));

        // No battery dir at all → None (desktop).
        let empty = base.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(read_battery(&empty), None);
        let _ = std::fs::remove_dir_all(&base); // best-effort: test tmp cleanup
    }

    #[test]
    fn read_battery_power_computes_watts_and_eta() {
        let base = std::env::temp_dir().join(format!("tg-battp-{}", std::process::id()));
        let bat = base.join("BAT0");
        let _ = std::fs::remove_dir_all(&base); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&bat).unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        // Discharging at 10 W with 20 Wh left → 2 h = 7200 s.
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat.join("power_now"), "10000000\n").unwrap(); // µW
        std::fs::write(bat.join("energy_now"), "20000000\n").unwrap(); // µWh
        std::fs::write(bat.join("energy_full"), "50000000\n").unwrap();
        assert_eq!(read_battery_power(&base), (Some(10.0), Some(7200)));

        // Charging: ETA is time to FULL (30 Wh needed at 10 W → 3 h).
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        assert_eq!(read_battery_power(&base), (Some(10.0), Some(10800)));

        // current/voltage fallback when power_now is absent: 2 A · 12 V = 24 W;
        // 24 Ah-equivalent... use charge tree: 12 Ah left at 2 A → 6 h.
        let _ = std::fs::remove_file(bat.join("power_now")); // best-effort: fixture file removal
        let _ = std::fs::remove_file(bat.join("energy_now")); // best-effort: fixture file removal
        let _ = std::fs::remove_file(bat.join("energy_full")); // best-effort: fixture file removal
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat.join("current_now"), "2000000\n").unwrap(); // µA
        std::fs::write(bat.join("voltage_now"), "12000000\n").unwrap(); // µV
        std::fs::write(bat.join("charge_now"), "12000000\n").unwrap(); // µAh
        assert_eq!(read_battery_power(&base), (Some(24.0), Some(21600)));

        // Idle / no power info → no watts, no eta.
        let empty = base.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(read_battery_power(&empty), (None, None));
        let _ = std::fs::remove_dir_all(&base); // best-effort: test tmp cleanup
    }

    #[test]
    fn steady_state_reads_are_bounded() {
        let dir = fixture("steady");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "73\n"),
                ("energy_now", "20000000\n"),
                ("energy_full", "50000000\n"),
                ("power_now", "10000000\n"),
                ("current_now", "1000000\n"),
                ("voltage_now", "12000000\n"),
            ],
        );
        supply(dir.path(), "AC", &[("type", "Mains"), ("online", "0\n")]);
        let now = Instant::now();
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        assert_eq!(reader.sample(now).battery, Some((73, false)));
        let before = reader.counters();
        reader.sample(now + Duration::from_secs(1));
        let after = reader.counters();
        assert_eq!(after.scans - before.scans, 0);
        assert_eq!(after.opens - before.opens, 0);
        assert!(after.reads - before.reads <= 6);
    }

    #[test]
    fn status_implying_ac_skips_adapter_reads() {
        let dir = fixture("ac-status");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Charging\n"),
                ("capacity", "80\n"),
            ],
        );
        for name in ["AC", "AC1", "AC2"] {
            supply(dir.path(), name, &[("type", "Mains"), ("online", "1\n")]);
        }
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        let reading = reader.sample(Instant::now());
        assert_eq!(reading.battery, Some((80, true)));
        assert_eq!(reader.counters().adapter_reads, 0);
    }

    #[test]
    fn two_batteries_combine() {
        let dir = fixture("combined");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "40\n"),
                ("energy_now", "20000000\n"),
                ("energy_full", "50000000\n"),
                ("power_now", "10000000\n"),
            ],
        );
        supply(
            dir.path(),
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "60\n"),
                ("energy_now", "30000000\n"),
                ("energy_full", "50000000\n"),
                ("power_now", "5000000\n"),
            ],
        );
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        let reading = reader.sample(Instant::now());
        assert_eq!(reading.battery, Some((50, false)));
        assert_eq!(reading.power_w, Some(15.0));
        assert_eq!(reading.eta_secs, Some(12_000));
    }

    #[test]
    fn device_scope_battery_is_ignored() {
        let dir = fixture("device-scope");
        supply(
            dir.path(),
            "00-peripheral",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("status", "Discharging\n"),
                ("capacity", "5\n"),
            ],
        );
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "73\n"),
            ],
        );
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        assert_eq!(reader.sample(Instant::now()).battery, Some((73, false)));
    }

    #[test]
    fn vanished_supply_triggers_rediscovery() {
        let dir = fixture("vanished");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "20\n"),
            ],
        );
        let now = Instant::now();
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        assert_eq!(reader.sample(now).battery, Some((20, false)));
        std::fs::remove_dir_all(dir.path().join("BAT0")).unwrap();
        supply(
            dir.path(),
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "81\n"),
            ],
        );
        // Model the ENODEV/ENOENT read error that removed sysfs supplies produce.
        reader.fail_next_read_for_test();
        assert_eq!(
            reader.sample(now + Duration::from_secs(1)).battery,
            Some((81, false))
        );
        assert_eq!(reader.counters().scans, 2);
    }

    #[test]
    fn optional_power_read_error_keeps_battery_percent() {
        let dir = fixture("power-read-error");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "73\n"),
                ("energy_now", "20000000\n"),
                ("energy_full", "50000000\n"),
                ("power_now", "10000000\n"),
            ],
        );
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        assert_eq!(reader.sample(Instant::now()).battery, Some((73, false)));
        let scans = reader.counters().scans;
        reader.fail_next_power_read_for_test();

        assert_eq!(reader.sample(Instant::now()).battery, Some((73, false)));
        assert_eq!(reader.counters().scans, scans);
    }

    #[test]
    fn adapters_are_not_read_when_there_are_no_batteries() {
        let dir = fixture("adapter-only");
        supply(dir.path(), "AC", &[("type", "Mains"), ("online", "1\n")]);
        let mut reader = BatteryReader::new(dir.path().to_path_buf());

        assert_eq!(reader.sample(Instant::now()).battery, None);
        assert_eq!(reader.counters().adapter_reads, 0);
    }

    #[test]
    fn charge_eta_does_not_require_voltage_or_power() {
        let dir = fixture("charge-eta-no-voltage");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "50\n"),
                ("charge_now", "3000000\n"),
                ("current_now", "1500000\n"),
            ],
        );
        let mut reader = BatteryReader::new(dir.path().to_path_buf());

        let reading = reader.sample(Instant::now());

        assert_eq!(reading.power_w, None);
        assert_eq!(reading.eta_secs, Some(7200));
    }

    #[test]
    fn rediscovers_after_interval() {
        let dir = fixture("interval");
        supply(
            dir.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "70\n"),
            ],
        );
        let now = Instant::now();
        let mut reader = BatteryReader::new(dir.path().to_path_buf());
        assert_eq!(reader.sample(now).battery, Some((70, false)));
        supply(
            dir.path(),
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Discharging\n"),
                ("capacity", "30\n"),
            ],
        );
        assert_eq!(
            reader.sample(now + Duration::from_secs(59)).battery,
            Some((70, false))
        );
        assert_eq!(
            reader.sample(now + Duration::from_secs(61)).battery,
            Some((50, false))
        );
    }

    #[test]
    fn byte_parser_trims_and_rejects_invalid_values() {
        assert_eq!(linux::parse_u64(b" \t73\r\n"), Some(73));
        assert_eq!(linux::parse_u64(b""), None);
        assert_eq!(linux::parse_u64(b"not-a-number\n"), None);
    }
}
