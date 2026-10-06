# THE-731 plan

Scope: crates/thegn-host/src/daemon/service.rs worktree_create, worktree_lifecycle.rs.
Approach: 4 phases. (1) spawn_blocking, no DB: repo/base/branch/path + in-flight branch reservation.
(2) short with_db: slug + trust approvals snapshot. (3) spawn_blocking, no DB: PreCreate, git worktree add,
submodule init, blocking PostCreate (rollback on failure; nothing registered yet).
(4) short with_db: put_worktree, link_issue, default folder.
Lifecycle gets \*\_with_approvals variants so hooks need no Db handle.
Concurrency: static reservation set dedupes same-name creates; git add refuses leftover races cleanly.
Tests: reservation unit test. Out of scope: control API shape unchanged.
