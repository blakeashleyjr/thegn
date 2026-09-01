# THE-27 revision 1 completion

## Implemented

- Handoff `p` now derives the thread from the actual selected Conversation or
  expanded Files row and rejects non-thread rows; regression coverage covers
  cursor movement and non-thread selection.
- Headless handoff for both `p` and `P` now opens the existing confirmation
  menu, parks the exact selection during confirmation, and dispatches only
  after explicit confirmation. Cancellation clears the pending handoff. The
  existing off-loop agent run and completion waker pulse are preserved.
- Inline PR review rows in both `PrView` and `DiffView` render every comment
  in a thread with the existing wrapped/indented body treatment. Outdated
  blocks also retain every comment. Multi-comment tests cover both projections.
- `DiffView` ignores source `Tab` while structural output is active, keeping
  the Worktree source label paired with the structural body until the user
  returns to the internal view. A mode/source regression test covers this.
- OpenSpec design, proposal, panel spec, and task wording now document the
  implemented `p`/`P` contract and complete off-loop review snapshot
  cache/fetch behavior.

## Disputed

None.

## Verification

- Passed: `cargo nextest run -p thegn-host pr_view diff_view` (16 tests).
- Passed: `cargo nextest run -p thegn-host
headless_handoff_menu_requires_an_explicit_confirmation` (1 test).
- Passed: `just quick thegn-host`.
- Passed: `cargo clippy -p thegn-host --tests -- -D warnings`.
- Passed: commit-time pre-commit `treefmt` hooks for all revision commits.
- No e2e, `just test`, `just ci`, migration, or live-state binary invocation
  was run, per the revision instructions.

## Unverified

- Direct `treefmt` could not run: the environment lacks the `taplo` formatter.
- OpenSpec strict validation was not run because `openspec` is unavailable on
  PATH.
- Live forge, pane, and headless-agent integration remains unexercised.
