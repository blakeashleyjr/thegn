# THE-732 plan
Scope: crates/thegn-host/src/handlers/worktree_delete.rs only.
Approach: a missing sidebar_status.git entry (unhydrated / first scan failed) is Unknown, never clean; Unknown and submodule-dirty take the warning menu even with confirm_delete = false. Clean verified targets still delete silently.
Tests: pure target_state unit test. Out of scope: degraded rescan keeping last-known values (bool only), core worktree::remove force escalation.
