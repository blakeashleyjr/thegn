# Show PR review feedback in diffs and hand it to an agent

Linear: THE-27

## Why

The forge seam already returned full review threads and the in-app PR view
already rendered them in Conversation, but neither the PR Files tab nor the
full-screen changes surface projected that feedback onto the PR-head diff.
Reviewers had to switch contexts to understand a line comment. The same thread
also needed a safe, human-triggered path into the worktree's agent.

## What Changes

- Added one identity-bearing, complete PR review snapshot per worktree: PR-head
  diff plus top-level comments/reviews and full threads, cached best-effort in
  schema v63 and never overwritten by a partial fetch.
- Added pure exact new-side anchoring with explicit outdated/general buckets;
  feedback is never guessed onto a nearby or local-worktree line.
- Rendered inline threads in the PR Files tab and in an explicit **PR review**
  source of the full-screen diff. The existing **Worktree** source remains the
  default and keeps local/staging semantics.
- Added unresolved counts, unresolved-first navigation, a view-local resolved
  toggle, replies from selectable thread rows, and honest loading/stale/
  unsupported states.
- Added `p` for the selected thread and `P` for all unresolved feedback. A live
  recognized agent pane in the active worktree receives bounded sanitized text
  as a non-submitting paste; otherwise a configured headless PR-review task is
  confirm-gated and run off-loop under the existing isolation-floor policy.

## Safety and compatibility

- Remote review bodies are bounded, data-delimited, stripped of terminal
  controls, and passed through bracketed-paste terminator hardening.
- Live paste has no trailing newline and never targets a pane outside the active
  worktree. Headless handoff carries the PR-review rules and never adds a new
  forge-write operation.
- Existing literal glyph cleanup and broader UI polish are implementation
  maintenance, not part of the persisted review-feedback contract.

## Delivered evidence

- Substrate/cache: `90187ba1`.
- Review rows and handoff seams: `11f1f6a7`.
- Host integration: `9f8cd03e`.
- Final reviewed merge: `b3aff883`.

## Non-goals

- Anchoring PR-head comments onto the local uncommitted Worktree diff.
- Resolving threads, approving, merging, or auto-submitting as a handoff side
  effect.
- Autonomous watched-PR dispatch; that is THE-22 and consumes this snapshot and
  formatter as delivered substrate.
- Adding a vendor-specific forge endpoint.
