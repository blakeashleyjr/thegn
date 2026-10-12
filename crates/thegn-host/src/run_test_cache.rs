//! Loop-local fencing and worker-side reads for per-worktree test cache state.

use crate::panel::{PanelUi, TestCache};

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

    /// Claim the synchronous path used by Tests entry and explicit run start.
    /// A previously applied cache is already at least as current as SQLite.
    pub(crate) fn begin_sync(&mut self, worktree: &str) -> bool {
        self.pending = None;
        self.loaded_for.as_deref() != Some(worktree)
    }

    pub(crate) fn finish_sync(&mut self, worktree: &str) {
        self.loaded_for = Some(worktree.to_owned());
    }
}

pub(crate) fn load_test_cache(db: &thegn_core::db::Db, worktree: &str) -> Option<TestCache> {
    use thegn_core::store::CacheStore as _;

    db.get_test_cache(worktree)
        .ok()
        .flatten()
        .and_then(|(json, _)| serde_json::from_str(&json).ok())
}
