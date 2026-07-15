# Cross-host merge queue (fold branches from other machines)

## Summary

The merge queue folds every queued branch into **one target repo's object
store** — the host where the target branch (`main`) lives. Until now every part
of that path assumed the target store and *every* queued worktree sat on the
local machine: queue rows carried no host, repo-membership was resolved by
shelling `git worktree list` on the local FS (so a queued worktree on another
machine resolved to nothing and was **silently dropped from the drain**), and a
remote branch's tip OID — which lives only in that branch host's own store —
could never be folded because it was absent from the target store.

This change makes the queue **host-aware** so a single queue can span machines
(e.g. `main` + some branches on `machine0`, more worktrees on a laptop):

1. **Self-describing rows.** Each `merge_queue` row records the worktree's
   `location` (mirrored from `worktrees.location` at enqueue), so a row can be
   attributed to a host without a live git shell.
2. **Host-independent membership.** A row belongs to a repo by its DB-recorded
   `repo_path`, not a local `git worktree list` — a worktree on another host is
   attributed to its repo instead of being dropped.
3. **Cross-host tip ingest.** When a queued branch's worktree lives on a
   different host from the target, its tip is **bundle-fetched** into the target
   store (a `git bundle` created on the branch's host, streamed over, fetched
   under `refs/thegn/mq/<branch>`) before the object-DB fold merges it. An
   unreachable branch host **defers** the row with a reason — never a silent
   drop — and is retried on the next drain.
4. **Target-host anchoring.** The fold/gate/CAS must run co-located with the
   target repo. When the drain is invoked from a host other than the target's,
   thegn refuses with guidance to run it on the target host (where step 3 fetches
   any off-host branches in). The panel shows each row's host as an `@host` chip.

## Impact

- Roadmap: tasks.md **J (Remote access)** — extends the remote-worktree model
  (already host-aware for the read panels) to the merge queue's write path.
- Spec: `merge-queue` — MODIFIED membership/drain requirements; ADDED cross-host
  ingest + target-host anchoring.
- Code: `merge_queue.location` column (schema v44, additive); `merge_ops`
  (`target_loc`, `remote_target_guard`); `merge_remote` (bundle ingest);
  host-independent `merge_driver::rows_for_repo`; `attempt_land` takes the
  branch's `GitLoc` and folds a fetched `refs/thegn/mq/*` ref for off-host
  branches; new `AttemptOutcome::Unreachable`. Local-only behavior is unchanged
  (a same-host branch yields `refs/heads/<branch>` with no I/O).

## Rationale

The read panels already run every git/`gh` read through `GitLoc` (local / ssh /
provider), so a remote worktree's diff and PR render like a local one. The merge
queue lagged: it is the one write path, and its object-DB fold is intrinsically
tied to a single store. Rather than teach the fold to reach across stores over
ssh per-operation (slow, and the throwaway gate worktree + CAS don't port
cleanly), the queue is **anchored to the target store's host** and only the
*branch tips* cross the wire — the smallest thing that must move — via the same
git-bundle mechanism the remote-worktree sync already uses.

## Non-goals

- **Auto-dispatching the drain to a remote host.** Having the local UI transpar-
  ently RPC a merge-drain daemon on the target host over ssh/iroh needs remote-
  daemon reach that is only partially built (tasks.md J128/129). Until then the
  supported workflow is to run the drain on the target host; thegn detects a
  remote target and says which host to run it on.
- Changing the fold/gate/CAS engine, conflict/agent handoff, or the read panels.
