The repository-wide open-PR list already uses the root checkout as its cache
key. Use the cached fetch timestamp for durable freshness and a process-local
repo-keyed claim to collapse overlapping worktree refreshes before the cache
write completes. The existing `[watch].pr_interval_secs` value supplies the
TTL. Current-branch PR panels remain keyed by worktree because their results
depend on branch and checkout scope.
