//! Loop-local fencing and worker-side reads for per-worktree test cache state.

use crate::panel::{PanelUi, TestCache};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Default)]
pub(crate) struct TestCacheLoad {
    seq: u64,
    pending: Option<(String, u64)>,
    loaded_for: Option<String>,
}

impl TestCacheLoad {
    pub(crate) fn begin_switch(&mut self, worktree: &str) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.pending = Some((worktree.to_owned(), self.seq));
        self.loaded_for = None;
        self.seq
    }

    pub(crate) fn is_pending(&self, worktree: &str) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(pending_worktree, _)| pending_worktree == worktree)
    }

    pub(crate) fn accept(
        &mut self,
        worktree: &str,
        seq: u64,
        current_worktree: &str,
        ui: &mut PanelUi,
        cache: Option<TestCache>,
    ) -> bool {
        let Some((pending_worktree, pending_seq)) = self.pending.as_ref() else {
            return false;
        };
        if current_worktree != worktree || pending_worktree != worktree || *pending_seq != seq {
            return false;
        }
        self.pending = None;
        self.loaded_for = Some(worktree.to_owned());
        if let Some(cache) = cache {
            ui.tests.apply_cache(cache);
        }
        true
    }

    /// Claim the async path used by Tests entry and explicit run start.
    /// A previously applied cache is already at least as current as SQLite.
    pub(crate) fn begin_sync(&mut self, worktree: &str) -> bool {
        if self.is_pending(worktree) {
            return false;
        }
        self.pending = None;
        self.loaded_for.as_deref() != Some(worktree)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutcomeDisposition {
    Apply,
    Defer,
    DropOtherWorktree,
    DropStaleGeneration,
}

impl OutcomeDisposition {
    pub(crate) fn clears_active_latch(self) -> bool {
        matches!(self, Self::Apply | Self::DropStaleGeneration)
    }
}

pub(crate) fn outcome_disposition(
    outcome_worktree: &str,
    outcome_generation: u64,
    active_worktree: &str,
    current_generation: u64,
    cache_pending: bool,
) -> OutcomeDisposition {
    if outcome_worktree != active_worktree {
        OutcomeDisposition::DropOtherWorktree
    } else if outcome_generation != current_generation {
        OutcomeDisposition::DropStaleGeneration
    } else if cache_pending {
        OutcomeDisposition::Defer
    } else {
        OutcomeDisposition::Apply
    }
}

pub(crate) fn spawn_test_cache_load(
    fence: &mut TestCacheLoad,
    worktree: &str,
    tx: &UnboundedSender<(String, u64, Option<TestCache>)>,
    waker: &TerminalWaker,
) {
    let seq = fence.begin_switch(worktree);
    let worktree = worktree.to_owned();
    let tx = tx.clone();
    let waker = waker.clone();
    crate::db_task::persist(move |db| {
        let cache = load_test_cache(db, &worktree);
        if let Err(error) = tx.send((worktree, seq, cache)) {
            tracing::debug!(target: "thegn::db", %error, "test cache load receiver is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::db", %error, "test cache load wake failed");
        }
    });
}

pub(crate) fn load_test_cache(db: &thegn_core::db::Db, worktree: &str) -> Option<TestCache> {
    use thegn_core::store::CacheStore as _;

    db.get_test_cache(worktree)
        .ok()
        .flatten()
        .and_then(|(json, _)| serde_json::from_str(&json).ok())
}
