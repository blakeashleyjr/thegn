//! Battery state as `(percent 0–100, on AC)`. sysinfo does not cover battery,
//! so this is ours: a native sysfs reader on Linux (which can distinguish a
//! charge-capped-but-plugged-in battery from one that is truly discharging),
//! and `starship-battery` elsewhere.

/// Read battery state: `(capacity %, on AC)`. `None` when there is no battery
/// (desktop / VM). The bool means "plugged in", not "actively charging".
#[cfg(target_os = "linux")]
pub fn read_battery(base: &std::path::Path) -> Option<(u8, bool)> {
    read_battery_sysfs(base)
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
    read_battery_power_sysfs(base)
}

#[cfg(not(target_os = "linux"))]
pub fn read_battery_power(_base: &std::path::Path) -> (Option<f32>, Option<u64>) {
    read_battery_power_starship()
}

/// Linux `/sys/class/power_supply` reader. AC presence comes from an adapter's
/// `online` flag — the only signal that survives charge-limiting, since a
/// battery capped at e.g. 80% reports `Discharging` even while plugged in. The
/// battery's own `Charging`/`Full`/`Not charging` status is kept as a fallback
/// for trees that expose no adapter `online` file. Pure given a base dir, so
/// it's unit-testable against a fixture tree.
#[cfg(target_os = "linux")]
fn read_battery_sysfs(base: &std::path::Path) -> Option<(u8, bool)> {
    let mut battery: Option<(u8, bool)> = None; // (capacity, status implies AC)
    let mut ac_online = false;
    for e in std::fs::read_dir(base).ok()?.flatten() {
        let p = e.path();
        // Batteries advertise type "Battery"; adapters say "Mains" (or "USB").
        if std::fs::read_to_string(p.join("type"))
            .map(|t| t.trim() == "Battery")
            .unwrap_or(false)
        {
            if battery.is_some() {
                continue; // first battery wins
            }
            let pct = std::fs::read_to_string(p.join("capacity"))
                .ok()?
                .trim()
                .parse::<u8>()
                .ok()?;
            let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
            let status_ac = matches!(status.trim(), "Charging" | "Full" | "Not charging");
            battery = Some((pct.min(100), status_ac));
        } else if std::fs::read_to_string(p.join("online"))
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
        {
            // A Mains/USB adapter reporting online=1 means we're plugged in,
            // regardless of whether the battery is actually taking charge.
            ac_online = true;
        }
    }
    let (pct, status_ac) = battery?;
    Some((pct, ac_online || status_ac))
}

/// Linux power/ETA reader over the same `/sys/class/power_supply` tree. Watts
/// come from `power_now` (µW) when present, else `current_now` × `voltage_now`.
/// Time-to-empty/full is `energy` ÷ `power` (µWh/µW → hours), falling back to
/// `charge` ÷ `current` (µAh/µA) on trees that expose charge instead of energy.
/// Pure given a base dir, so it's fixture-testable like [`read_battery_sysfs`].
#[cfg(target_os = "linux")]
fn read_battery_power_sysfs(base: &std::path::Path) -> (Option<f32>, Option<u64>) {
    let dir = match std::fs::read_dir(base) {
        Ok(d) => d,
        Err(_) => return (None, None),
    };
    for e in dir.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type"))
            .map(|t| t.trim() != "Battery")
            .unwrap_or(true)
        {
            continue; // first battery wins; skip adapters
        }
        let num = |name: &str| -> Option<f64> {
            std::fs::read_to_string(p.join(name))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()
        };
        // Power in µW: direct, or current(µA)·voltage(µV)/1e6.
        let power_uw = num("power_now")
            .or_else(|| Some(num("current_now")?.abs() * num("voltage_now")? / 1_000_000.0));
        let charging = std::fs::read_to_string(p.join("status"))
            .map(|s| s.trim() == "Charging")
            .unwrap_or(false);
        // Remaining/needed energy in µWh (energy tree), else charge in µAh with
        // current in µA — both give hours when divided by their rate.
        let (remaining, rate) = if let Some(en) = num("energy_now") {
            let target = if charging {
                num("energy_full").map(|f| (f - en).max(0.0))
            } else {
                Some(en)
            };
            (target, power_uw)
        } else if let Some(ch) = num("charge_now") {
            let target = if charging {
                num("charge_full").map(|f| (f - ch).max(0.0))
            } else {
                Some(ch)
            };
            (target, num("current_now").map(f64::abs))
        } else {
            (None, power_uw)
        };
        let watts = power_uw
            .filter(|w| *w > 0.0)
            .map(|w| (w / 1_000_000.0) as f32);
        let eta = match (remaining, rate) {
            (Some(r), Some(rate)) if rate > 0.0 => Some((r / rate * 3600.0) as u64),
            _ => None,
        };
        return (watts, eta);
    }
    (None, None)
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
}
