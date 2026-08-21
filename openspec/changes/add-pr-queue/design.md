# Design — PR queue

## Event loop, rendering, schema

- **Wake path:** one new `RefreshKind::PrQueue`, driven by a slot multiple in the
  existing `spawn_refresh_ticker` OS thread (`hydrate.rs`), plus an immediate
  kick from `git_watch::is_remote_ref_path` (a push already fires `Pr`/`Ci`; the
  queue joins them so a push re-evaluates now rather than in a minute). Fetches
  run on `crate::sched::spawn_bg` and pulse the waker **once** per pass. No
  polling is added to the idle path — the ticker already exists, and the poller
  is skipped entirely while `[pr_queue] enabled = false`.
- **Damage channels:** the panel section is chrome, so a row change sets the
  master `dirty` (a `Full` frame) exactly like the merge-queue section. Driver
  transitions stream over a channel and patch the row in place, mirroring
  `handlers/merge_queue.rs::apply_step`.
- **SQLite:** new `pr_queue` table ⇒ **`user_version` bump**, additive, with a
  migration for existing DBs.
- **Help context key:** `panel:prq` → `docs/help/pr-queue.md`, which also claims
  the three new action ids (the ratchet requires it).

## Keyed by repo + PR number, not by worktree

The merge-queue row is keyed by worktree path, because a queued branch always has
one. A PR does not: you may want to shepherd a PR whose branch was never checked
out locally. So the key is `<repo_root>#<number>` and `worktree` is nullable.

The consequence is explicit in `decide`: an action that needs a checkout (running
the agent) on a row with no worktree resolves to `NeedsHuman` with that reason,
rather than being silently skipped.

## Classification is pure; the driver only executes

`pr_queue::classify(&PrStatus, cfg) -> Blocker` and
`pr_queue::decide(blocker, facts, cfg) -> QueueAction` are I/O-free and
exhaustively unit-tested (core's 95% gate). Everything that matters on a team —
the safety rules below — is a _decision_, so it is covered by table tests rather
than being an emergent property of driver control flow.

`classify` needs no new fetching: `is_draft`, `merge_state_status`
(`DIRTY`/`BEHIND` ⇒ conflict, `BLOCKED` ⇒ review/checks), the `check_bucket`
rollup, and `review_decision` all already ride on `PrStatus` from
`pr_status_full`.

## Team-safety rules (the substantive difference from the merge queue)

A solo merge queue can be blunt: it owns the branch and the target. A PR lives in
shared space, so each of these is encoded in `decide` and tested:

1. **Never stomp a teammate.** The agent pushes with `--force-with-lease`. If the
   remote head moved and thegn did not move it, `pause_on_foreign_push` yields
   `NeedsHuman` instead of pushing.
2. **The forge owns the merge.** Default `merge_mode = "auto_merge"` sets the
   forge's own auto-merge, so required reviews, required checks, and any
   server-side merge queue stay authoritative — thegn's view of "green" can never
   race branch protection.
3. **The attempt budget resets on a new head OID.** A PR lives for days; the
   merge queue's one-shot budget would exhaust and never recover. Reset is keyed
   on `last_head_oid` changing, so an agent looping on its own pushes does _not_
   refill it (thegn records the OID it produced).
4. **`own_prs_only`.** Watching a teammate's PR is fine; writing to it is not.
5. **Reply, never resolve.** Resolution is the reviewer's judgement.
6. **Never merge a draft**, and never merge with `require_approval` unmet.
7. **Enqueue is explicit** (`auto_enqueue = "off"`), and the whole feature is
   **off by default** — it is the one part of the shell that makes network writes.

## Status vocabulary

`watching` · `blocked_ci` · `blocked_conflict` · `blocked_review` ·
`agent_running` · `ready` · `merging` · `merged` · `needs_human` · `closed`.

Terminal-but-not-final states (`blocked_*`) are re-evaluated every poll, which is
the difference from the merge queue's drain-once model: the PR queue is a
**watcher** that occasionally acts, not a batch job.

## Agent dispatch reuses the shared engine

Three new `TaskKind`s (`PrCiFailure`, `PrConflict`, `PrReview`) with their own
variable sets and built-in prompts; dispatch goes through
`thegn_core::agent_task` + `crate::agent_run` unchanged. The PR queue therefore
inherits the quoting contract, the watchdog, the git-env scrub, and the single
Windows stub for free — which is why the engine was extracted first.

Its prompts state the inverse rule from the merge queue's: the agent **must**
push (that is how a PR advances) and must **not** merge the PR.
