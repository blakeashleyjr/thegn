# THE-89 security/test/bug review

PASS

The branch was reviewed after merging `main` and against the full
`git diff main...HEAD`. The architect checklist is covered: classification is
line-based and off the compositor loop, the event snapshot precedes delta
consumption, cache entries are daemon-generation scoped and wake the model,
and configured signatures are bounded and substring-only (no regex execution).

Two review fixes were committed:

- `cd61e464` gates classification to agent sessions, rejects excessive
  signature counts, and prevents stale bridge generations from overwriting or
  clearing newer state.
- `b6d9c9ab` adds the agent-path tool-call-noise acceptance regression.

Verification, all with temporary `XDG_STATE_HOME` values:

- mandated core filter: 513 passed;
- mandated host filter: 121 passed;
- focused THE-89 tests: 9 core and 13 host passed, plus the final acceptance
  set of 3 host tests;
- `XDG_RUNTIME_DIR=/tmp just quick thegn-core`: passed;
- `XDG_RUNTIME_DIR=/tmp just quick thegn-host`: passed;
- `cargo clippy -p thegn-host --tests`: passed;
- `git diff --check`: passed.

The lane changes control/event frames (`SessionInfo.error_active` and
`SessionActivityEvent.worktree/error_active`) and can change the existing
sidebar Failure glyph. No current Muse snapshot exercises an active THE-89
error state, so no existing snapshot requires re-recording; a future visual
fixture for this state should add a dedicated sidebar snapshot.

Manual live-agent and e2e checks were not run, per the scoped review request.
The merge step remains `thegn integrate`.
