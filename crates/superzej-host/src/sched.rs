//! Background-work scheduling gate.
//!
//! The whole app shares one `spawn_blocking` pool (`max_blocking_threads`).
//! Without a cap, background refreshes — disk `du`, PR/issue/CI/my-work network
//! fetches — can occupy every pool thread and delay the *interactive* model
//! hydration a worktree switch is waiting on (the "usable performance is always
//! first" invariant). Background tasks reserve a permit from a small semaphore
//! before doing their blocking work; interactive tasks (model hydration, panel
//! prefetch) never do, so they always find pool headroom. A background task that
//! can't get a permit simply skips this round — its periodic trigger retries.

use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrency cap for background `spawn_blocking` work — a quarter of the
/// runtime's blocking pool (`max_blocking_threads(32)` in `main.rs`), leaving the
/// large majority for interactive hydration and user-initiated git actions.
const BG_PERMITS: usize = 8;

fn bg_sem() -> &'static Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| Arc::new(Semaphore::new(BG_PERMITS)))
}

/// Try to reserve a background slot. `Some(permit)` → run the work, holding the
/// permit until it drops (end of the task). `None` → the background lane is full;
/// skip this round (periodic refreshes self-heal on the next tick). Non-blocking
/// and safe from any thread — meant as the first line of a background
/// `spawn_blocking` closure: `let Some(_permit) = crate::sched::bg_permit() else { return; };`.
pub fn bg_permit() -> Option<OwnedSemaphorePermit> {
    bg_sem().clone().try_acquire_owned().ok()
}

/// Run `f` on the blocking pool **through the background lane**: a drop-in for
/// `tokio::task::spawn_blocking` for non-interactive work. Reserves a
/// [`bg_permit`] first; if the lane is full the closure is skipped this round
/// (periodic refreshes retry). Interactive work (model hydration, panel prefetch)
/// keeps calling `spawn_blocking` directly so it always has pool headroom.
pub fn spawn_bg<F: FnOnce() + Send + 'static>(f: F) {
    tokio::task::spawn_blocking(move || {
        let Some(_permit) = bg_permit() else {
            return; // background lane full — periodic trigger retries next tick
        };
        f();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_are_bounded_and_released_on_drop() {
        // Exhaust the lane, confirm the next reservation is refused, then release
        // one and confirm a slot frees up.
        let held: Vec<_> = (0..BG_PERMITS).map(|_| bg_permit()).collect();
        assert!(
            held.iter().all(Option::is_some),
            "all {BG_PERMITS} permits granted"
        );
        assert!(
            bg_permit().is_none(),
            "lane is full — further reservations refused"
        );
        drop(held);
        assert!(bg_permit().is_some(), "a released permit frees a slot");
    }
}
