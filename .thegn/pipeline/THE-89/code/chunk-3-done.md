# THE-89 chunk 3 completion

Implemented the architect revision for bootstrap, lifetime, wake, and noise-boundary correctness.

- Added `SessionInfo.error_active` to the authoritative daemon session listing and used it to seed the host cache after the event bridge subscribes, before buffered activity deltas are consumed.
- Added generation-scoped cache ownership, snapshot replacement, owner-scoped `SessionExit` handling, and disconnect cleanup so a stale bridge cannot clear a newer daemon connection or leave a glyph active indefinitely.
- Connected cache transitions to the existing `RefreshKind::Model` channel and `TerminalWaker`; unchanged values are coalesced.
- Removed broad shipped `authentication failed` and `permission denied` substrings. Explicit configured harness signatures remain supported, while ordinary tool-call permission/auth output is regression-tested as non-failure.
- Added focused cache lifetime/snapshot/wake tests and updated the gRPC session schema projection.

## Commit

`e347a785` — `fix(thegn-host): complete daemon error cache lifecycle (THE-89)`

## Validation

- `cargo fmt --all -- --check` — passed.
- `XDG_STATE_HOME=/tmp/thegn-review-state-the89-c3 RUSTC_WRAPPER= cargo nextest run -p thegn-core agent_error` — 8 passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.
- Lead-mandated core filter with isolated `XDG_STATE_HOME` — 512 passed.
- Lead-mandated host filter with isolated `XDG_STATE_HOME` — 120 passed.
- Focused host filter (`agent_error_cache`, `error_state_lifecycle`, `agent_error_active`) — 10 passed.

## Unverified

- Manual live-agent verification of rendered glyph lighting/clearing was not run.
- Full-workspace gates (`just test`, `just coverage`, `just ci`) and e2e were not run, per the scoped dev-loop policy and chunk instructions.
