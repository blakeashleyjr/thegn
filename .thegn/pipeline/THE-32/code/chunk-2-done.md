# Chunk 2 completion

Implemented the service/host submodule seam and lifecycle under the exact
commit subject requested:

- Added batched submodule state, raw gitlink diff, summary, initialization, and
  conflict operations through GitBackend, including bridge/local fallback
  handling and glyph-batch dirty evidence.
- Replaced positional glyph-cache rows with a named record while retaining
  legacy eight-element array deserialization; propagated submodule_dirty
  through hydration, warm-cache restore, and sidebar state.
- Added validated submodule LOC boundaries while keeping physical disk
  accounting inclusive and de-duplicated only at registered worktree roots.
- Routed CLI, wizard, tracker, and daemon worktree creation through the shared
  host lifecycle. Clone, provider, and remote bundle materialization now honor
  effective submodule mode, recursive initialization, and a distinct
  repo-trust URL/path request. Initialization remains off-loop and non-fatal
  after checkout creation.
- Added dependent fixture/cache compatibility updates required by the named
  glyph record and chunk-1 DiffFile field.

## Verification

- just quick thegn-svc — passed.
- cargo nextest run -p thegn-svc submodule — 2 passed.
- cargo nextest run -p thegn-svc plumbing — 7 passed.
- just quick thegn-host — passed.
- cargo nextest run -p thegn-host glyph_scan — 5 passed.
- cargo nextest run -p thegn-host measure — 16 passed.
- cargo nextest run -p thegn-host workspace_create — 7 passed.
- just quick thegn-core — passed.
- git diff --check — passed.

## Unverified

- The broad cargo nextest run -p thegn-host worktree filter stopped after 52
  tests because the pre-existing silent-daemon timeout test hit
  PermissionDenied while creating its socket; 51 tests passed before that
  failure.
- No e2e, migration, built-binary, or full-workspace gate was run, per policy.
