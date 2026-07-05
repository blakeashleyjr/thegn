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

/// Non-Linux battery via `starship-battery` (macOS IOKit, Windows
/// `GetSystemPowerStatus`, BSD). Reports the first battery; "on AC" is true when
/// it is charging/full or a charger is attached.
#[cfg(not(target_os = "linux"))]
fn read_battery_starship() -> Option<(u8, bool)> {
    use starship_battery::{Manager, State};
    let manager = Manager::new().ok()?;
    let mut batteries = manager.batteries().ok()?;
    let bat = batteries.next()?.ok()?;
    // state_of_charge is a ratio 0.0..=1.0 (a `Ratio` quantity).
    let pct = (bat.state_of_charge().value * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    let on_ac = matches!(bat.state(), State::Charging | State::Full);
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn read_battery_parses_fixture_tree() {
        let base = std::env::temp_dir().join(format!("sz-batt-{}", std::process::id()));
        let bat = base.join("BAT0");
        let ac = base.join("AC");
        let _ = std::fs::remove_dir_all(&base);
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
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_battery_power_computes_watts_and_eta() {
        let base = std::env::temp_dir().join(format!("sz-battp-{}", std::process::id()));
        let bat = base.join("BAT0");
        let _ = std::fs::remove_dir_all(&base);
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
        let _ = std::fs::remove_file(bat.join("power_now"));
        let _ = std::fs::remove_file(bat.join("energy_now"));
        let _ = std::fs::remove_file(bat.join("energy_full"));
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat.join("current_now"), "2000000\n").unwrap(); // µA
        std::fs::write(bat.join("voltage_now"), "12000000\n").unwrap(); // µV
        std::fs::write(bat.join("charge_now"), "12000000\n").unwrap(); // µAh
        assert_eq!(read_battery_power(&base), (Some(24.0), Some(21600)));

        // Idle / no power info → no watts, no eta.
        let empty = base.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(read_battery_power(&empty), (None, None));
        let _ = std::fs::remove_dir_all(&base);
    }
}
