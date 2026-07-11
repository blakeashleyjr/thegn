# Add the agent-driven merge-queue driver (fold-actor autopilot)

## Summary

Stop babysitting merges into local `main`. The fold-actor merge queue
(`thegn-core/fold.rs` + host `integrate.rs`) already folds worktree branches
into the target branch in the object DB, test-gates the union, and CAS-advances
the ref — but a branch that **conflicts** or **breaks the gate** is only
_deferred_, leaving the user to hand-rebase, resolve, and fix the build. The two
config knobs meant to automate that (`conflict_handoff = "agent"`,
`auto_drain`) were declared but wired to nothing.

This change makes the queue **assignable and self-draining**:

1. **A single-branch land primitive** (`integrate::attempt_land`) that folds one
   branch onto the current target tip, gates it, and (when `auto_land`)
   CAS-advances — returning a rich outcome (`Landed`/`Ready`/`Conflict`/
   `GateFailed`/`UpToDate`) the driver routes on. Shares the fold/gate/CAS/resync
   machinery with the batch `run_fold`; the batch path is unchanged.
2. **A serial driver** (`merge_driver::drive_queue`) that drains queued branches
   one at a time: clean ones land; a conflict or red gate dispatches a **headless
   CLI agent** (Claude Code by default, any arbitrary command) **inside the
   branch's own worktree** to rebase/resolve/fix, then re-attempts — up to
   `agent_max_attempts`, marking `needs_human` if it gives up. The agent never
   merges into the target; thegn does the object-DB fold + CAS itself, so the
   coherence guarantee and the `merge_guard` hook are preserved.
3. **A `merge` CLI namespace** — `merge add [<wt>…|--all]`, `list`, `rm`,
   `clear`, `drain [--all]`, `land` — the programmatic assign-and-drain surface
   (`--json`, per the blanket CLI convention). The batch `integrate` command
   stays as the fold-everything-at-once path.
4. **Config** — `agent_command` (template with `{prompt}`/`{branch}`/`{target}`),
   `auto_land`, `agent_max_attempts`, `agent_timeout_secs` on `[merge_queue]`.

## Impact

- tasks.md: **T (263 Approve→merge, 268 squash/rebase pre-merge)** and the
  orchestration core **Q** "merge pipeline + queue" — the local, AI-driven land
  loop. Builds directly on the existing fold-actor (commit `4fbc92b`).
- **thegn-core** — 4 new `MergeQueueConfig` fields (docs condensed to hold the
  `config.rs` ratchet ceiling); `MergeQueueRow: Serialize` for `--json`; new
  statuses `agent_running`/`ready`/`needs_human` (free-text, no schema change).
- **thegn-host** — new `merge_driver.rs` + `cmd/merge.rs`; `integrate.rs`
  gains `attempt_land` and a sequenced throwaway-worktree path (`tmp_path`, fixes
  a seconds-resolution collision); panel `merge_queue` glyphs for the new
  statuses; `merge` added to the grouped-help table.
- **No new event-loop wake path** — the driver runs off-loop (CLI direct; a host
  `spawn_blocking` path can reuse it). The in-TUI actions + `auto_drain` wiring
  are deferred: `run.rs`/`keymap.rs` are at their god-file ratchet ceilings, so
  new `Action` variants need a prior extraction (tracked as a follow-up).

## Rationale

The fold engine is mature and I/O-free; the only missing piece between "the
queue defers a conflict" and "the conflict is resolved" was a driver that hands
the branch to an agent and re-folds. Doing the agent's work in the _worktree_
(rebase/resolve) and the _land_ in thegn (object-DB fold + CAS) keeps the
strong coherence guarantee the fold-actor was built for, while turning the
deferral into an automatic land.

## Non-goals

- **The agent merging into `main`** — it only makes its branch clean; thegn
  lands it. The `merge_guard` hook still refuses in-sandbox canonical merges.
- **Parallel agents** — the driver is serial (one branch at a time).
- **Pushing to a remote** — the driver lands to the local target only.
- **In-TUI keybindings / auto_drain** — deferred behind a `run.rs`/`keymap.rs`
  extraction; the CLI is the programmatic surface for now.
