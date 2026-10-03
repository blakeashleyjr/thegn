# THE-697/698 plan

- worktree::remove already serialises per repo (flock) and the sidebar path passes delete_branch=false; issue partly stale.
- Add remove_detailed (git stderr), is_lock_contention, bounded retry on contention only; destroy_one surfaces reason.
- Tests: core unit tests.
- THE-698 optimistic hide: not implemented, needs-decision.
