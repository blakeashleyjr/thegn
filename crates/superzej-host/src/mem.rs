//! Allocator hygiene for the long-running host process.
//!
//! The host is glibc-targeted (the Nix release build and the aarch64-darwin
//! host build are the two real targets). glibc's malloc spins up to
//! `8 × ncpu` per-thread *arenas* (≈192 on a 24-core box), each grows to its
//! own high-water mark and is **never returned to the OS**. With ~60 live
//! threads (tokio workers + the on-demand `spawn_blocking` pool + the per-tick
//! `std::thread::scope` git fan-out) this let RSS balloon to ~2.5 GB of dirty
//! arena memory that an audit traced to ~131 distinct arenas.
//!
//! Two cheap, pure-Rust (libc FFI only — no compiled C) levers fix it:
//!   * [`tune_allocator`] caps the arena count at startup, so memory can't
//!     sprawl across dozens of arenas.
//!   * [`trim_if_idle`] hands freed pages back to the OS once the loop has been
//!     idle for a bit, so RSS actually *recedes* after a build/test burst.
//!
//! Both are no-ops off glibc (musl static builds, non-Linux), so the calls
//! compile everywhere and simply do nothing where the knobs don't exist.

use std::time::{Duration, Instant};

/// Default cap on glibc malloc arenas. 2 is the conventional "server" value:
/// it collapses the per-thread arena sprawl while leaving enough arenas to
/// avoid serializing our (light, I/O-bound) allocation traffic on one lock.
const DEFAULT_ARENA_MAX: i32 = 2;

/// Only trim once the loop has gone this long without a real (non-`Skip`)
/// frame — i.e. genuinely idle, not mid-interaction.
const IDLE_TRIM_DELAY: Duration = Duration::from_secs(10);

/// Floor between trims so a slow idle wake cadence can never turn `malloc_trim`
/// (a few ms walking the heap) into a recurring cost.
const TRIM_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Cap the process's glibc arena count. Call this as the very first thing in
/// `main()` — before the tokio runtime spawns any threads — so every arena
/// created afterward honors the cap. Reads `SUPERZEJ_MALLOC_ARENA_MAX`
/// (default [`DEFAULT_ARENA_MAX`]); `0` disables the cap (glibc default).
pub fn tune_allocator() {
    let max = std::env::var("SUPERZEJ_MALLOC_ARENA_MAX")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(DEFAULT_ARENA_MAX);
    if max <= 0 {
        return;
    }
    set_arena_max(max);
}

/// Return freed arena pages to the OS, but only when the loop is actually idle
/// and not more often than [`TRIM_MIN_INTERVAL`]. Called on every loop wake;
/// it self-throttles, so it adds no timer and never runs on the hot path
/// (a wake that rendered a real frame resets `last_activity`, so we skip).
pub fn trim_if_idle(last_activity: Instant, last_trim: &mut Instant) {
    if last_activity.elapsed() >= IDLE_TRIM_DELAY && last_trim.elapsed() >= TRIM_MIN_INTERVAL {
        trim_now();
        *last_trim = Instant::now();
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn set_arena_max(max: i32) {
    // SAFETY: `mallopt` is a thread-safe glibc tunable; we call it once at
    // startup before any worker threads exist. A non-success return just means
    // the knob was ignored, which is harmless.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, max);
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_now() {
    // SAFETY: `malloc_trim` is thread-safe; `0` releases all trimmable top-of-
    // arena free space back to the OS. Return value (1 = freed memory) unused.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn set_arena_max(_max: i32) {}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_now() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_only_when_idle_and_not_too_often() {
        // Fresh activity → never trims (loop is busy).
        let mut last_trim = Instant::now() - Duration::from_secs(60);
        let active = Instant::now();
        trim_if_idle(active, &mut last_trim);
        assert!(
            last_trim.elapsed() >= Duration::from_secs(59),
            "recent activity must not trigger a trim"
        );

        // Idle long enough AND past the trim floor → trims (last_trim moves up).
        let mut last_trim = Instant::now() - Duration::from_secs(60);
        let idle = Instant::now() - Duration::from_secs(20);
        trim_if_idle(idle, &mut last_trim);
        assert!(
            last_trim.elapsed() < Duration::from_secs(5),
            "sustained idle past the floor must trim and reset last_trim"
        );

        // Idle, but trimmed recently → throttled, no trim.
        let mut last_trim = Instant::now();
        let idle = Instant::now() - Duration::from_secs(20);
        let before = last_trim;
        trim_if_idle(idle, &mut last_trim);
        assert_eq!(
            before, last_trim,
            "trim floor must throttle back-to-back trims"
        );
    }

    #[test]
    fn tune_allocator_is_callable() {
        // Smoke: the env path + FFI call must not panic. (No-op off glibc.)
        unsafe {
            std::env::set_var("SUPERZEJ_MALLOC_ARENA_MAX", "0");
        }
        tune_allocator();
        unsafe {
            std::env::remove_var("SUPERZEJ_MALLOC_ARENA_MAX");
        }
    }
}
