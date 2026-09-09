# Tasks — attention signal noise (THE-68)

## Delivered scope

- [x] Centralize the repo-inbox visibility predicate and use it for both display
      and scoped clear-all behavior, including unknown/main-checkout paths.
- [x] Add bounded `session_attention` live-state storage, pruning, worktree
      deletion cleanup, and configuration/env support for the opt-in audit row.
- [x] Replace default per-turn notification appends with one live attention row
      per session; lower it on input, teardown, acknowledgement, and clear-all.
- [x] Preserve deliberate `agent_attention` pushes and make the opt-in inbox row
      current-per-session rather than append-only.
- [x] Fold live rows into existing blocked/needs-user scoring, longest-waiting
      ordering, hydration, and UI indicators without a new render path.
- [x] Cover notification scope, SQL operations/migration, config, daemon signal/
      input races, scoring, ack/clear, opt-in, and deliberate push behavior.
- [x] Update delta specs, help prose, and changelog.
- [x] Land the implementation and race fixes (`c899eae5`, `770c2136`,
      `690103aa`, `3d8125cb`) and strictly validate this change.

## Validation boundary

The task file was not updated as chunks landed. Completion here reflects code,
tests, review, and history; it does not claim a new interactive/e2e run.
