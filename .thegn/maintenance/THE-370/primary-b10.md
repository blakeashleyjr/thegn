# THE-370 + THE-374 — primary instructions (batch 10, codex restart)

Urgent pair: never delete user work when a destructive Git precondition is UNKNOWN. A previous agent
started this lane and died; its UNCOMMITTED changes (crates/thegn-core/src/util.rs, crates/thegn-host/src/
integrate.rs, crates/thegn-svc/src/git/{stage,undo}.rs and maybe more) may be partial — `git diff`
first, keep what is right, then complete. Its plan (if present) is .thegn/maintenance/THE-370/agent-plan.md.

THE-370 (data): automatic post-land and expiry cleanup treats a failed `git status --porcelain` as
"not dirty" then destroys with `worktree remove --force`.

- crates/thegn-host/src/merge_lifecycle.rs `worktree_is_dirty` returns false on git_out None;
  merge_sweep.rs relies on it; thegn-core util.rs helper cannot distinguish clean from failure.
- Replace with a typed `Clean | Dirty(details) | Unknown(error)`. Automatic AND forced sweep/reclaim
  retain the worktree, branch and a retryable lifecycle record for Dirty and Unknown ("force" may bypass
  age only, never uncertainty). Avoid check-then-force races: revalidate immediately before removal and
  abort on any difference (do not rely on two independent probes). Surface bounded diagnostics.
  THE-374 (data): fail closed on destructive Git preconditions.
- crates/thegn-svc/src/git/undo.rs: is_dirty(...).unwrap_or(false) skips autostash before reset --hard.
  Unknown must refuse the undo.
- crates/thegn-svc/src/git/stage.rs: any cat-file error => "absent from HEAD" => git rm -f. Distinguish
  the precise "valid repo, valid HEAD, path absent" exit from any execution/repository error; refuse on error.
- crates/thegn-host/src/integrate.rs ~539-574 `CliGit::is_dirty(...).unwrap_or(false)` and the boolean
  `git_ok(merge-base --is-ancestor)` at ~1074-1081 and ~1341-1350: unknown dirty state excludes/fails the
  candidate; ancestor probes return typed Yes/No/Unknown and Unknown aborts before any ref/worktree mutation.
- Audit other `unwrap_or(false)` / `is_ok()` / `is_none()` branches that select a destructive Git action;
  fix the ones you find, list any you leave in your report.

Scope limits: THE-368 (typed bounded git execution) and THE-372 (generation-fenced transactions) are NOT
landed — do the typed precondition + refuse-on-unknown + revalidate-before-destroy within existing
helpers; do not build the full THE-372 transaction. Another batch-10 lane (THE-689) edits
crates/thegn-host/src/worktree_lifecycle.rs and session.rs (destroy admission) — avoid those files.
Tests: fault-inject a failing git (e.g. fake git on PATH / non-repo dir / unreadable index) for each
path: no destructive command runs and the worktree/branch/files survive; a genuinely clean worktree is
still reclaimed. Git test fixtures must use `-c commit.gpgsign=false`. Ignored Results need
`// best-effort:` comments; never swallow errors on the primary path of a destructive action.
