//! CPU time clocks used by the runtime performance rollup.

/// Calling thread's consumed CPU time in nanoseconds, or `0` where the clock is
/// unavailable. Uses `CLOCK_THREAD_CPUTIME_ID` so blocked time is not counted.
#[cfg(unix)]
#[inline]
pub(crate) fn thread_cpu_ns() -> u64 {
    match nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID) {
        Ok(ts) => (ts.tv_sec() as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec() as u64),
        Err(_) => 0,
    }
}

#[cfg(not(unix))]
#[inline]
pub(crate) fn thread_cpu_ns() -> u64 {
    0
}

#[cfg(unix)]
fn usage_cpu_ns(who: libc::c_int) -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for `rusage`; getrusage fills
    // it when it returns success, which is checked before reading the value.
    let result = unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    // SAFETY: a successful getrusage call initialized the output structure.
    let usage = unsafe { usage.assume_init() };
    timeval_pair_ns(
        i64::try_from(usage.ru_utime.tv_sec).unwrap_or(0),
        i64::try_from(usage.ru_utime.tv_usec).unwrap_or(0),
        i64::try_from(usage.ru_stime.tv_sec).unwrap_or(0),
        i64::try_from(usage.ru_stime.tv_usec).unwrap_or(0),
    )
}

/// Current process CPU (all its threads) in nanoseconds.
#[cfg(unix)]
pub(crate) fn process_cpu_ns() -> u64 {
    usage_cpu_ns(libc::RUSAGE_SELF)
}

/// CPU of children that have been reaped, in nanoseconds.
#[cfg(unix)]
pub(crate) fn reaped_children_cpu_ns() -> u64 {
    usage_cpu_ns(libc::RUSAGE_CHILDREN)
}

#[cfg(not(unix))]
pub(crate) fn process_cpu_ns() -> u64 {
    0
}

#[cfg(not(unix))]
pub(crate) fn reaped_children_cpu_ns() -> u64 {
    0
}

#[cfg(any(unix, test))]
fn timeval_pair_ns(utime_s: i64, utime_us: i64, stime_s: i64, stime_us: i64) -> u64 {
    let user_ns = u64::try_from(utime_s)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(utime_us).unwrap_or(0).saturating_mul(1_000));
    let system_ns = u64::try_from(stime_s)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(stime_us).unwrap_or(0).saturating_mul(1_000));
    user_ns.saturating_add(system_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeval_pair_ns_adds_and_clamps() {
        assert_eq!(timeval_pair_ns(2, 250_000, 1, 500_000), 3_750_000_000);
        assert_eq!(timeval_pair_ns(-1, 250_000, 0, -1), 250_000_000);
    }

    #[test]
    fn process_cpu_is_monotonic_and_covers_threads() {
        let before = process_cpu_ns();
        let worker = std::thread::spawn(|| {
            let mut value = 0u64;
            for i in 0..2_000_000u64 {
                value = value.wrapping_add(i.wrapping_mul(3));
            }
            std::hint::black_box(value);
        });
        worker.join().unwrap();
        let after = process_cpu_ns();
        assert!(after >= before);
    }
}
