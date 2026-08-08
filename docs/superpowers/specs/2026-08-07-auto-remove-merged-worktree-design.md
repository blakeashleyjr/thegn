# Auto-remove a worktree once its branch lands cleanly

**Date:** 2026-08-07
**Status:** Approved (design)

## Summary

When a branch lands cleanly through the thegn merge queue, remove its worktree
and delete the now-merged branch — **by default**. This is a first-class,
default-on behavior gated by an existing config knob, so users who want to keep
worktrees or branches can opt back out with one word.

The capability already exists and is fully wired (CLI + in-app land paths, live
tab teardown, safety guards, home-checkout protection). It is currently **off by
default** and buried under a master switch. This change flips two defaults and
rewrites the accompanying docs — no new mechanism.

## Motivation

After a branch merges, its worktree is dead weight: it lingers as a sidebar tab
and on disk, and the merged branch ref is redundant (it's already in `main`).
Today, clearing the merge queue (`thegn merge clear`) only drops the queue
*assignment*, not the worktree — so merged work stays visible until the user
manually closes each tab. Auto-removing on a clean land keeps the workspace to
"live work only" without manual cleanup.

## Current state (what already exists)

`[merge_queue]` config, `crates/thegn-core/src/config.rs`:

- `organize_folders: bool` (default `false`) — **master toggle** for the whole
  sidebar lifecycle. When off, `decide()` returns `Noop` for every event, so the
  entire feature (including worktree removal) is inert.
- `on_landed: OnLanded` (default `Off`) — what to do when a branch lands:
  - `off` — nothing
  - `move` — file the worktree into `merged_folder`
  - `detach` — remove the worktree, **keep** the branch
  - `remove` — remove the worktree **and delete** the merged branch
- `queued_folder` (`"Merging"`), `merged_folder` (`"Merged"`),
  `failed_folder` (`"Needs attention"`) — sidebar folder names.

The pure decision is `thegn_core::merge_lifecycle::decide()`; the I/O half is
`crates/thegn-host/src/merge_lifecycle.rs` (`apply` → `remove_landed` →
`thegn_core::worktree::remove`). The land status transition to `"landed"` (which
fires `LifecycleEvent::Landed`) happens only on a **clean** fold in
`merge_driver.rs:160` — never on `Conflict` / `GateFailed` / `Unreachable`. Live
tabs whose worktree was removed are reaped by `reconcile_removed_tabs` (on the
loop). The home/main checkout is explicitly exempt
(`merge_lifecycle.rs:33`), and a failed removal (read-only mount, uncommitted
changes) is a logged warning with the DB row kept — never a crash.

## Decision

Approach A of the three considered: **flip the two existing defaults** rather
than add a new alias key or decouple removal from `organize_folders`.

- Rejected **B (add `remove_merged_worktrees` alias)**: a second knob that can
  disagree with `on_landed`; redundant surface.
- Rejected **C (decouple removal from `organize_folders`)**: the user chose to
  enable the whole lifecycle, not just removal.

## Design

### Changes

1. `crates/thegn-core/src/config.rs` — in `impl Default for MergeQueueConfig`:
   - `organize_folders: false → true`
   - `on_landed: OnLanded::Off → OnLanded::Remove`
2. `config/config.toml.example` (the `[merge_queue]` sidebar-organization block,
   ~lines 2800–2815) — flip the two documented values and rewrite the
   "OFF by default" prose to describe the new default-on lifecycle.
3. Doc comment on `on_landed` in `config.rs` stays accurate ("only when
   `organize_folders = true`") since `organize_folders` is now true by default.

No changes to `decide()`, `remove_landed()`, `worktree::remove()`, the
land-status transition, or tab reaping.

### Resulting default behavior

- **Enqueued** → worktree filed into the `Merging` sidebar folder.
- **Landed cleanly** → worktree removed **and** branch deleted (`git branch -D`);
  a live tab is torn down like a manual close.
- **Failed** (conflict / red gate / agent gave up) → worktree filed into the
  `Needs attention` folder.
- Home/main checkout never touched; failed removal logs a warning and keeps the
  DB row.

### The config knob (unchanged, now default-on)

`[merge_queue] on_landed`: `remove` (new default) | `detach` (keep branch) |
`move` (keep worktree, file into `Merged`) | `off` (leave in place). The master
off-switch is `organize_folders = false`, which reverts to today's inert
behavior.

## Scope / non-goals

- **Only** branches landed through thegn's merge queue (`drain` / `land` /
  `integrate`) trigger this. A branch merged elsewhere (GitHub PR, manual
  `git merge`) does **not** auto-clean its worktree. Out of scope.
- The default now creates `Merging` / `Needs attention` folders in every
  merge-queue user's sidebar and force-deletes merged branches. This is the
  intended "whole lifecycle on" behavior, called out explicitly so it is not a
  surprise.

## Testing

- `thegn_core::merge_lifecycle` unit tests use explicit configs, so existing
  cases are unaffected. Add a test asserting the **default** config yields
  `RemoveWorktree { delete_branch: true }` on `Landed`, `FileInto("Merging")`
  on `Enqueued`, and `FileInto("Needs attention")` on `Failed`.
- Grep for any test that constructs `MergeQueueConfig::default()` and assumed
  the old inert behavior; adjust as needed.
- Gate: `just quick` while iterating; `just test` before it leaves the machine
  (the pre-push heavy gate). Core coverage stays ≥95%.
