//! Thread quality-of-service — how the OS scheduler is told what a thread is
//! *for*.
//!
//! macOS and Linux act on this, with platform-specific scheduler hints. On
//! Apple silicon QoS influences whether a thread is eligible for performance
//! cores or is steered to efficiency cores. A process that says nothing runs
//! everything at the default class, so background hydration, metrics polling,
//! fs-watching and git fan-out all compete for P-cores with the render loop —
//! burning battery and generating heat for work nobody is waiting on.
//!
//! thegn is unusually exposed to this: it is a compositor whose whole
//! performance story is "~0% idle", with ~100 `thread::spawn`/`spawn_blocking`
//! sites in this crate alone. The render/input loop must stay at the default
//! (interactive) class; everything off the loop is, by construction, work the
//! user is not blocking on.
//!
//! Deliberately a **thread-self** call rather than a spawn-time attribute:
//! `pthread_set_qos_class_self_np` is the only supported way to set this (macOS
//! exposes no "set another thread's QoS"), and tokio's blocking pool hands us
//! threads we did not spawn. So each worker declares its own class on entry.
//!
//! Linux applies per-thread nice and best-effort I/O priority to dedicated
//! threads. Those values are inherited by threads and child processes created
//! afterwards, and an unprivileged thread cannot undo a nice increase. Runtime
//! pool threads are therefore left alone. Windows and other platforms remain
//! no-ops; macOS uses its reversible thread QoS API.

/// What a thread is for, in scheduler terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qos {
    /// Work the user is actively waiting on: the render/input loop. The default
    /// class — set explicitly only to *restore* it.
    Interactive,
    /// Work the user started and will notice the result of, but is not blocked
    /// on: model hydration, a git fan-out behind a visible panel, a pane spawn.
    Utility,
    /// Housekeeping the user never asked for and would not miss: the metrics
    /// sampler, fs-watch registration, cache cleanup, orphan GC.
    Background,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Qos;

    // From <sys/qos.h>. Not in the `libc` crate's darwin bindings, so the values
    // are spelled out; they are ABI-stable (a public, versioned system header).
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    const QOS_CLASS_UTILITY: u32 = 0x11;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(
            qos_class: u32,
            relative_priority: std::os::raw::c_int,
        ) -> std::os::raw::c_int;
        // Test-only: nothing reads a scheduling hint back at runtime — it exists
        // to prove the setter actually lands (see `current`).
        #[cfg(test)]
        fn qos_class_self() -> u32;
    }

    /// The calling thread's current class, as one of ours. `None` for a class we
    /// never set (e.g. `QOS_CLASS_UNSPECIFIED` on a workqueue thread). Test-only:
    /// the point of a scheduling hint is that nothing reads it back at runtime.
    #[cfg(test)]
    pub(super) fn current() -> Option<Qos> {
        // SAFETY: a libSystem call with no arguments, reading the CALLING
        // thread's own class.
        match unsafe { qos_class_self() } {
            QOS_CLASS_USER_INTERACTIVE => Some(Qos::Interactive),
            QOS_CLASS_UTILITY => Some(Qos::Utility),
            QOS_CLASS_BACKGROUND => Some(Qos::Background),
            _ => None,
        }
    }

    pub(super) fn apply(qos: Qos) {
        let class = match qos {
            Qos::Interactive => QOS_CLASS_USER_INTERACTIVE,
            Qos::Utility => QOS_CLASS_UTILITY,
            Qos::Background => QOS_CLASS_BACKGROUND,
        };
        // SAFETY: a libSystem call taking two scalars and affecting only the
        // CALLING thread. `relative_priority` must be <= 0; 0 is the default.
        // Failure (EPERM on a thread with a workqueue-managed class) is a
        // best-effort no-op — scheduling is a hint, never a correctness input.
        unsafe {
            pthread_set_qos_class_self_np(class, 0);
        }
    }
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinuxSched {
    nice_offset: i32,
    io_be_level: Option<u8>,
}

#[cfg(any(test, target_os = "linux"))]
fn linux_sched(qos: Qos) -> LinuxSched {
    match qos {
        Qos::Interactive => LinuxSched {
            nice_offset: 0,
            io_be_level: None,
        },
        Qos::Utility => LinuxSched {
            nice_offset: 5,
            io_be_level: None,
        },
        Qos::Background => LinuxSched {
            nice_offset: 10,
            io_be_level: Some(7),
        },
    }
}

#[cfg(any(test, target_os = "linux"))]
fn target_nice(baseline: i32, current: i32, qos: Qos) -> Option<i32> {
    let target = baseline
        .saturating_add(linux_sched(qos).nice_offset)
        .clamp(-20, 19);
    (target > current).then_some(target)
}

#[cfg(any(test, target_os = "linux"))]
const POOLED_RUNTIME_THREAD_NAME: &str = "thegn-rt";

#[cfg(any(test, target_os = "linux"))]
fn is_pooled_thread(name: Option<&str>) -> bool {
    name.is_some_and(|name| {
        name == POOLED_RUNTIME_THREAD_NAME || name.starts_with("tokio-runtime-worker")
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Qos, is_pooled_thread, linux_sched, target_nice};
    use std::io;

    pub(super) fn priority(who: libc::id_t) -> Option<i32> {
        // SAFETY: `__errno_location` returns this thread's errno slot.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: getpriority reads the specified Linux thread's scheduling
        // priority; `who` is either this process's pid or a gettid result.
        let value = unsafe { libc::getpriority(libc::PRIO_PROCESS, who) };
        // SAFETY: the errno pointer is valid for this thread.
        let error = unsafe { *libc::__errno_location() };
        if value == -1 && error != 0 {
            tracing::debug!(target: "thegn::qos", %error, "could not read Linux thread nice value");
            None
        } else {
            Some(value)
        }
    }

    pub(super) fn apply(qos: Qos) {
        if is_pooled_thread(std::thread::current().name()) {
            return;
        }

        // SAFETY: gettid takes no arguments and returns the calling thread ID.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        if tid < 0 {
            let error = io::Error::last_os_error();
            tracing::debug!(target: "thegn::qos", %error, "could not read Linux thread id");
            return;
        }
        let Some(baseline) = priority(std::process::id()) else {
            return;
        };
        let Some(current) = priority(tid as libc::id_t) else {
            return;
        };

        if let Some(target) = target_nice(baseline, current, qos) {
            // SAFETY: setpriority applies a valid nice value to the calling
            // Linux thread identified by its tid.
            let result =
                unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, target) };
            if result != 0 {
                let error = io::Error::last_os_error();
                tracing::debug!(target: "thegn::qos", %error, target, "could not raise Linux thread nice value");
            }
        }

        if let Some(level) = linux_sched(qos).io_be_level {
            // Linux does not expose these ioprio constants through libc.
            const IOPRIO_WHO_PROCESS: libc::c_int = 1;
            const IOPRIO_CLASS_BE: libc::c_int = 2;
            const IOPRIO_CLASS_SHIFT: u32 = 13;
            let ioprio = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | i32::from(level);
            // SAFETY: syscall uses Linux's ioprio_set ABI, targeting only this
            // thread (IOPRIO_WHO_PROCESS with its tid).
            let result =
                unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio) };
            if result != 0 {
                let error = io::Error::last_os_error();
                tracing::debug!(target: "thegn::qos", %error, "could not set Linux thread I/O priority");
            }
        }
    }

    #[cfg(test)]
    #[expect(dead_code)]
    pub(super) fn current() -> Option<Qos> {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Qos;
    pub(super) fn apply(_qos: Qos) {}
    /// No class to read where there is no class to set.
    ///
    /// Kept for seam symmetry even though its only caller
    /// (`macos_actually_applies_the_requested_class`) is macOS-only, so on this
    /// side of the cfg it is compiled and never called.
    #[cfg(test)]
    #[expect(dead_code)]
    pub(super) fn current() -> Option<Qos> {
        None
    }
}

/// Declare what the **calling** thread is for. Best-effort and idempotent; call
/// it as the first statement in a worker's body. Linux applies per-thread
/// niceness (and Background I/O priority); new threads and child processes
/// inherit those settings. Nice can only be raised by an unprivileged thread,
/// so Linux leaves pooled runtime threads alone.
///
/// Never call this on the event loop with anything but [`Qos::Interactive`] — a
/// demoted render thread is exactly the frame-latency regression the perf
/// invariants exist to catch.
pub fn set_self(qos: Qos) {
    imp::apply(qos);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_a_class_is_best_effort_on_every_platform() {
        // The contract is "best-effort, never observable as a failure": macOS
        // and Linux make real system calls, unsupported platforms are no-ops.
        // Either way it must not panic. Keep Linux's one-way nice adjustment
        // off the shared test-runner thread.
        std::thread::spawn(|| {
            set_self(Qos::Interactive);
            set_self(Qos::Utility);
            set_self(Qos::Background);
            set_self(Qos::Interactive);
        })
        .join()
        .expect("worker thread must not panic setting its QoS");
    }

    #[test]
    fn linux_sched_maps_every_class() {
        assert_eq!(
            linux_sched(Qos::Interactive),
            LinuxSched {
                nice_offset: 0,
                io_be_level: None
            }
        );
        assert_eq!(
            linux_sched(Qos::Utility),
            LinuxSched {
                nice_offset: 5,
                io_be_level: None
            }
        );
        assert_eq!(
            linux_sched(Qos::Background),
            LinuxSched {
                nice_offset: 10,
                io_be_level: Some(7)
            }
        );
    }

    #[test]
    fn target_nice_only_ever_raises() {
        assert_eq!(target_nice(0, 0, Qos::Background), Some(10));
        assert_eq!(target_nice(0, 10, Qos::Utility), None);
        assert_eq!(target_nice(0, 0, Qos::Interactive), None);
        assert_eq!(target_nice(15, 15, Qos::Background), Some(19));
        assert_eq!(target_nice(0, 5, Qos::Background), Some(10));
    }

    #[test]
    fn pooled_runtime_threads_are_recognised() {
        assert!(is_pooled_thread(Some("thegn-rt")));
        assert!(is_pooled_thread(Some("tokio-runtime-worker")));
        assert!(is_pooled_thread(Some("tokio-runtime-worker-2")));
        assert!(!is_pooled_thread(Some("thegn-glyph-scan")));
        assert!(!is_pooled_thread(None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_actually_lowers_a_dedicated_thread() {
        std::thread::Builder::new()
            .name("thegn-qos-test".to_owned())
            .spawn(|| {
                // SAFETY: gettid reads the calling thread's Linux tid.
                let tid = unsafe { libc::syscall(libc::SYS_gettid) };
                assert!(tid >= 0);
                let start = imp::priority(tid as libc::id_t).unwrap();
                // The process ID identifies the main thread, whose nice value
                // is the baseline used by the implementation.
                let baseline = imp::priority(std::process::id()).unwrap();
                // SAFETY: ioprio_get reads this thread's I/O priority.
                let before_ioprio = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, tid) };
                set_self(Qos::Background);
                // SAFETY: ioprio_get reads this thread's I/O priority.
                let after_ioprio = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, tid) };
                let after = imp::priority(tid as libc::id_t).unwrap();
                assert_eq!(
                    after,
                    target_nice(baseline, start, Qos::Background).unwrap_or(start)
                );
                set_self(Qos::Utility);
                assert_eq!(
                    imp::priority(tid as libc::id_t).unwrap(),
                    after,
                    "a later class must not lower nice"
                );

                // A sandbox may allow ioprio_get while denying ioprio_set.
                // Assert the requested value only when the best-effort set
                // changed the value; otherwise the syscall may be unavailable.
                if before_ioprio >= 0 && after_ioprio >= 0 && before_ioprio != after_ioprio {
                    assert_eq!((after_ioprio as i32) & 0x1fff, 7);
                    assert_eq!((after_ioprio as i32) >> 13, 2);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pooled_threads_are_never_demoted() {
        std::thread::Builder::new()
            .name("thegn-rt".to_owned())
            .spawn(|| {
                // SAFETY: gettid reads the calling thread's Linux tid.
                let tid = unsafe { libc::syscall(libc::SYS_gettid) };
                assert!(tid >= 0);
                let before = imp::priority(tid as libc::id_t).unwrap();
                set_self(Qos::Background);
                assert_eq!(imp::priority(tid as libc::id_t), Some(before));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Proof the FFI actually lands, not just that it doesn't crash. Without
    /// this the whole module could be a no-op on its one supported platform and
    /// every other test would still pass.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_actually_applies_the_requested_class() {
        // On its own thread: this test must not leave the shared test-runner
        // thread demoted for whatever runs on it next.
        std::thread::spawn(|| {
            for want in [Qos::Background, Qos::Utility, Qos::Interactive] {
                set_self(want);
                assert_eq!(
                    imp::current(),
                    Some(want),
                    "pthread_set_qos_class_self_np did not take effect"
                );
            }
        })
        .join()
        .unwrap();
    }
}
