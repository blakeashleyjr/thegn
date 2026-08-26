//! Registry of child processes whose CPU and memory count as *thegn's own*.
//!
//! # Why this exists
//!
//! thegn's footprint reporting used to be two hardcoded PIDs: the compositor
//! (`self_rss_bytes` / `self_cpu_pct`) and the pane daemon. Everything else it
//! spawns was invisible. On one real session the compositor reported 507 MB
//! while its own direct children — three `gopls`, seven node language servers,
//! the `podman events` watcher — held **1,016 MB** that appeared in no metric.
//! thegn was under-reporting itself by roughly 3×.
//!
//! Adding `lsp_rss_bytes` beside `daemon_rss_bytes` would not have fixed that.
//! A user may run zero language servers or a hundred; plugins, watchers and
//! picker-launched tools are all still to come. The set is open, so the
//! accounting has to be a registry rather than a widening struct.
//!
//! # What belongs here
//!
//! Processes thegn spawns **for its own purposes** and whose cost a user would
//! reasonably attribute to thegn: language servers, plugin hosts, event
//! watchers, tool subprocesses.
//!
//! What does NOT belong here is anything running in a **pane**. Panes are the
//! user's own shells and programs — a build, a test run, an editor — and they
//! are owned by the pane daemon, not the compositor, so they sit outside this
//! registry structurally rather than by convention.
//!
//! # Lifecycle
//!
//! [`register`] returns a [`ProcHandle`] that deregisters on drop, so a
//! registration cannot outlive the thing that made it. Registration is also
//! only ever a *hint*: a process can die without its handle dropping (a crash,
//! a kill), so the sampler treats a PID it can no longer find as gone rather
//! than trusting this list. That keeps a stale entry from inventing memory.
//!
//! # Cost
//!
//! A `Mutex<Vec<_>>` touched on registration (rare — a server start) and once
//! per sampler tick. Nothing here wakes the event loop or allocates on the
//! render path, so the ~0%-idle invariant is untouched.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Language servers (`rust-analyzer`, `gopls`, `tsserver`, …).
pub const GROUP_LSP: &str = "lsp";
/// Long-lived event subscribers thegn keeps open (`podman events`).
pub const GROUP_WATCHER: &str = "watcher";
/// Plugin host processes.
pub const GROUP_PLUGIN: &str = "plugin";
/// Tools launched from the picker that outlive the invocation.
pub const GROUP_TOOL: &str = "tool";

/// One registered child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedProc {
    pub pid: u32,
    /// Category, used to roll up subitems in the UI. One of the `GROUP_*`
    /// constants, or any stable literal a future subsystem defines — the set is
    /// deliberately open so a new producer needs no change here.
    pub group: &'static str,
    /// Human label for this specific process, e.g. `"gopls · thegn"`. Shown
    /// only in the expanded per-process view; the rolled-up row uses `group`.
    pub label: String,
}

struct Entry {
    id: u64,
    proc: TrackedProc,
}

fn registry() -> &'static Mutex<Vec<Entry>> {
    static REG: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register a child process as thegn's own for resource accounting.
///
/// Drop the returned handle when the process is reaped. Registering the same
/// PID twice is harmless — the sampler deduplicates before it refreshes, which
/// it must do anyway (a repeated PID in a `sysinfo` targeted refresh aborts the
/// process over a file-descriptor double close; see `dedup_pids`).
#[must_use = "the process is deregistered as soon as the handle is dropped"]
pub fn register(group: &'static str, label: impl Into<String>, pid: u32) -> ProcHandle {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.push(Entry {
        id,
        proc: TrackedProc {
            pid,
            group,
            label: label.into(),
        },
    });
    ProcHandle { id }
}

/// Every currently-registered child, in registration order.
///
/// Read once per sampler tick. Order is stable so a UI list built from it does
/// not reshuffle between frames.
pub fn tracked() -> Vec<TrackedProc> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|e| e.proc.clone())
        .collect()
}

/// How many processes are registered. Cheaper than [`tracked`] when only the
/// count is wanted.
pub fn len() -> usize {
    registry().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Deregisters its process on drop.
///
/// Keyed by a monotonic id rather than the PID: PIDs are recycled by the OS, and
/// dropping a stale handle must never remove a *different* subsystem's newer
/// registration that happens to have inherited the number.
#[derive(Debug)]
pub struct ProcHandle {
    id: u64,
}

impl Drop for ProcHandle {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.retain(|e| e.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so tests that assert on its *contents*
    /// must not run beside each other. Everything below either serialises on
    /// this or asserts only about its own entries.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn register_then_drop_round_trips() {
        let _g = lock();
        let before = len();
        let h = register(GROUP_LSP, "gopls", 4242);
        let now = tracked();
        assert_eq!(now.len(), before + 1);
        let mine = now.iter().find(|p| p.pid == 4242).expect("registered");
        assert_eq!(mine.group, GROUP_LSP);
        assert_eq!(mine.label, "gopls");
        drop(h);
        assert_eq!(len(), before, "the handle deregisters on drop");
        assert!(!tracked().iter().any(|p| p.pid == 4242));
    }

    #[test]
    fn dropping_a_stale_handle_cannot_evict_a_recycled_pid() {
        let _g = lock();
        // The OS recycles PIDs. If handles were keyed by PID, dropping the first
        // handle here would silently deregister the SECOND subsystem's process.
        let first = register(GROUP_LSP, "old server", 777);
        let second = register(GROUP_PLUGIN, "new plugin", 777);
        drop(first);
        let now = tracked();
        let survivors: Vec<_> = now.iter().filter(|p| p.pid == 777).collect();
        assert_eq!(survivors.len(), 1, "only the stale entry is removed");
        assert_eq!(survivors[0].group, GROUP_PLUGIN);
        assert_eq!(survivors[0].label, "new plugin");
        drop(second);
    }

    #[test]
    fn duplicate_pids_are_allowed_and_left_for_the_sampler_to_dedup() {
        let _g = lock();
        // Two subsystems can legitimately name the same PID (a shared server).
        // The registry does not police it; the sampler must dedup before any
        // targeted refresh regardless, or sysinfo aborts on a double close.
        let a = register(GROUP_LSP, "a", 9001);
        let b = register(GROUP_LSP, "b", 9001);
        assert_eq!(tracked().iter().filter(|p| p.pid == 9001).count(), 2);
        drop(a);
        drop(b);
    }

    #[test]
    fn an_empty_registry_is_the_normal_zero_lsp_case() {
        let _g = lock();
        let h = register(GROUP_LSP, "temp", 1);
        drop(h);
        // A user running no language servers, no plugins and no watchers simply
        // has nothing here — the accounting must degrade to "self + daemon".
        assert!(tracked().iter().all(|p| p.pid != 1));
    }
}
