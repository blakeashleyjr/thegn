//! Host-local admission for heavyweight gate commands.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};
use thegn_core::fsperm::{PrivateDir, ensure_private_dir, ensure_private_dir_beneath};
use thegn_core::heavy_gate::{self, Config};

const HEAVY_WAIT_CAP: Duration = Duration::from_secs(30 * 60);
const INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const MAX_BACKOFF: Duration = Duration::from_secs(1);

/// A held heavy-gate slot. The advisory lock is released by dropping the file.
#[must_use = "dropping HeavySlot releases the host-local gate slot"]
pub(crate) struct HeavySlot {
    #[allow(dead_code)]
    directory: Option<PrivateDir>,
    #[allow(dead_code)]
    lock: Option<std::fs::File>,
}

impl HeavySlot {
    fn inert() -> Self {
        Self {
            directory: None,
            lock: None,
        }
    }
}

/// Wait for and acquire a host-local heavy-gate slot.
pub(crate) fn acquire(cfg: &Config, label: &str) -> Result<HeavySlot> {
    acquire_with(
        cfg,
        label,
        &thegn_core::util::xdg_state_home(),
        HEAVY_WAIT_CAP,
        &read_loadavg1,
        &crate::fold_cancel::requested,
    )
}

fn acquire_with(
    cfg: &Config,
    label: &str,
    state_home: &Path,
    wait_cap: Duration,
    loadavg1: &dyn Fn() -> Option<f64>,
    cancelled: &dyn Fn() -> bool,
) -> Result<HeavySlot> {
    if cfg.slots == 0 && cfg.max_load <= 0.0 {
        return Ok(HeavySlot::inert());
    }

    let directory = heavy_directory(state_home)?;
    let deadline = Instant::now() + wait_cap;
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if cancelled() {
            anyhow::bail!("heavy gate wait for {label} cancelled");
        }
        let (holders, candidate) = try_slots(&directory, cfg.slots)?;
        let load = loadavg1().unwrap_or(0.0);
        match heavy_gate::decide(cfg.slots, cfg.max_load, holders, load, cores) {
            heavy_gate::Decision::Admit => {
                return Ok(HeavySlot {
                    directory: Some(directory),
                    lock: candidate,
                });
            }
            heavy_gate::Decision::WaitLoad | heavy_gate::Decision::WaitSlot => {
                drop(candidate);
            }
        }

        if Instant::now() >= deadline {
            tracing::warn!(
                target: "thegn::heavy_gate",
                label,
                slots = cfg.slots,
                max_load = cfg.max_load,
                "heavy gate wait cap reached; proceeding fail-open"
            );
            thegn_core::outln!(
                "heavy gate {label} waited 30 minutes; proceeding while the host is still busy"
            );
            return Ok(HeavySlot {
                directory: Some(directory),
                lock: None,
            });
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn heavy_directory(state_home: &Path) -> Result<PrivateDir> {
    // The state-home anchor may be absent on a first-run CLI invocation. Only
    // that creation path uses ensure_private_dir; an existing anchor is never
    // re-tightened by this gate.
    if !state_home.exists() {
        ensure_private_dir(state_home)
            .with_context(|| format!("could not create state home {}", state_home.display()))?;
    }
    ensure_private_dir_beneath(state_home, Path::new("thegn/heavy"))
        .context("could not secure heavy-gate state directory")
}

fn try_slots(directory: &PrivateDir, slots: usize) -> Result<(usize, Option<std::fs::File>)> {
    if slots == 0 {
        return Ok((0, None));
    }
    let mut holders = 0;
    let mut candidate = None;
    for index in 0..slots {
        let name = format!("heavy-{index}.lock");
        let file = directory
            .open_or_create_regular_nofollow(&name)
            .with_context(|| format!("could not open heavy-gate lock {name}"))?;
        match file.try_lock() {
            Ok(()) if candidate.is_none() => candidate = Some(file),
            Ok(()) => drop(file),
            Err(std::fs::TryLockError::WouldBlock) => holders += 1,
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Ok((holders, candidate))
}

fn read_loadavg1() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|load| load.is_finite() && *load >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::{Config, acquire_with};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn second_guard_waits_until_first_drops() {
        let state = tempfile::tempdir().unwrap();
        let cfg = Config {
            slots: 1,
            max_load: 0.0,
        };
        let first = acquire_with(
            &cfg,
            "first",
            state.path(),
            Duration::from_millis(500),
            &|| Some(0.0),
            &|| false,
        )
        .unwrap();
        let state_path = state.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            let slot = acquire_with(
                &cfg,
                "second",
                &state_path,
                Duration::from_millis(500),
                &|| Some(0.0),
                &|| false,
            )
            .unwrap();
            (slot, started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(first);
        let (slot, elapsed) = worker.join().unwrap();
        assert!(slot.lock.is_some());
        assert!(elapsed >= Duration::from_millis(40));
    }

    #[test]
    fn load_gate_retries_against_injected_load_average() {
        let state = tempfile::tempdir().unwrap();
        let cfg = Config {
            slots: 0,
            max_load: 1.0,
        };
        let released = Arc::new(AtomicBool::new(false));
        let worker_released = Arc::clone(&released);
        let state_path = state.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            acquire_with(
                &cfg,
                "load",
                &state_path,
                Duration::from_millis(500),
                &|| {
                    if worker_released.load(Ordering::Acquire) {
                        Some(0.0)
                    } else {
                        Some(2.0)
                    }
                },
                &|| false,
            )
            .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        released.store(true, Ordering::Release);
        assert!(worker.join().unwrap().lock.is_none());
    }

    #[test]
    fn cancellation_aborts_waiting_for_a_slot() {
        let state = tempfile::tempdir().unwrap();
        let cfg = Config {
            slots: 1,
            max_load: 0.0,
        };
        let first = acquire_with(
            &cfg,
            "first",
            state.path(),
            Duration::from_millis(500),
            &|| Some(0.0),
            &|| false,
        )
        .unwrap();
        let result = acquire_with(
            &cfg,
            "cancelled",
            state.path(),
            Duration::from_millis(500),
            &|| Some(0.0),
            &|| true,
        );
        drop(first);
        assert!(result.is_err());
    }
}
