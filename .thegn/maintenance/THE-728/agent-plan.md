# THE-728 plan

Scope: (1) admission check inside Panes::spawn_argv_env / spawn_argv_env_local
(spawn_argv delegates), keyed on claimed worktree paths only; (2) cross-process
advisory flock per canonical worktree path under $XDG_STATE_HOME/thegn/locks.
Destroy: exclusive try-lock for the whole removal, refuse on failure. Pane open:
shared try-lock for the open only, refuse with "worktree is being removed".
Files: worktree_admission.rs (new), worktree_lifecycle.rs, panes.rs, platform/{unix,windows}.rs.
Out of scope: durable removal for stale layout snapshots written after a CLI
landing (needs a DB schema change; to be filed as a follow-up). Option B rejected.
Known limit: the pane open holds the lock only during the open, so a destroy that
starts after a pane is live is still governed by the session-latch check.
