# THE-89 chunk 1 — completion summary

**Branch:** `tg/the-89-error-glyph-tool-calls`

**Commits:**

- `da5bc859` — `feat(thegn-core): agent error classification + config key (THE-89)`
- `4da0778b` — `feat(thegn-core): agent error classification + config key (THE-89)`

The second commit adds the config-focused tests after the initial implementation
was committed early. Both commits use the exact subject required by the chunk
spec.

## Implemented

Exactly the chunk-1 `thegn-core` scope:

- Added `crates/thegn-core/src/agent_error.rs` with pure, case-insensitive
  harness-banner classification, shipped defaults, empty-list no-op behavior,
  and per-session error state lifecycle.
- Exported `agent_error` from `crates/thegn-core/src/lib.rs`.
- Added `[notifications].agent_error_signatures` with the shipped defaults and
  `[]` override behavior to `NotificationsConfig`.
- Added strict config validation rejecting empty signature entries and entries
  over 256 characters.
- Added `agent_error_active` to `AttentionInputs` and scored it as
  `Failure`/`AgentFailed` at sub-priority 3, below process failure and above
  CI failure.
- Added focused unit tests for classification, tool-call noise exclusions,
  state clear-on-resume, config defaults/override/validation, and attention
  scoring.

No daemon integration, control API changes, attention hydration plumbing, help
page edits, or other chunk-2 files were changed.

## Verification

- `cargo nextest run -p thegn-core agent_error` — **passed** (6 tests).
- `cargo nextest run -p thegn-core config::agent_error_signatures` — **passed**
  (2 tests).
- `just quick thegn-core` — **passed**.
- `treefmt` pre-commit hook — **passed** at both commits after formatting.
- `git diff --check` — **passed** before the final test-only commit.

The requested filter `cargo nextest run -p thegn-core attention::score_with_agent_error`
was also attempted, but nextest's filter syntax did not select the test; the
same test ran and passed as part of the `agent_error` filter.

## Unverified

- `just test`, `just ci`, `just coverage`, full-workspace clippy, cross/MSRV/
  feature gates, smoke, and e2e were not run per the lead addendum and the
  repository dev-loop policy.
- Coverage percentage for the new module was not measured; the new pure logic
  and config/attention paths have focused tests, but the 95% gate is deferred
  to the review/pre-PR stage.
- Chunk-2 daemon/session and host integration remain unverified here by design;
  chunk 2 must land after these commits.
