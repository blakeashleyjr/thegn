//! Thread quality-of-service — how the OS scheduler is told what a thread is
//! *for*.
//!
//! Only macOS acts on this today, and there it matters a lot: on Apple silicon
//! QoS is what decides whether a thread is eligible for the performance cores or
//! is steered to the efficiency cores. A process that says nothing runs
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
//! A no-op on every other platform: Linux's analogue is `nice`/`ionice`/cgroups,
//! which are process- or cgroup-scoped rather than per-thread and are already
//! handled by `sandbox_cpucap`; Windows has thread priorities with different
//! semantics. Neither is worth guessing at here.

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

#[cfg(not(target_os = "macos"))]
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
/// it as the first statement in a worker's body.
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
    fn setting_a_class_is_safe_and_reversible_on_every_platform() {
        // The contract is "best-effort, never observable as a failure": on macOS
        // this makes real libSystem calls, elsewhere it compiles to nothing.
        // Either way it must not panic, and a thread must be able to move
        // between classes (workers are reused by tokio's blocking pool).
        for q in [Qos::Background, Qos::Utility, Qos::Interactive] {
            set_self(q);
        }
        // And on a spawned thread, which is the real call site.
        std::thread::spawn(|| {
            set_self(Qos::Utility);
            set_self(Qos::Background);
        })
        .join()
        .expect("worker thread must not panic setting its QoS");
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
