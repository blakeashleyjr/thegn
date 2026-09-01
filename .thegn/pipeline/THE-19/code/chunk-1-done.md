# THE-19 chunk 1 completion

Implemented the substrate-free core lifecycle-hook policy.

- Added `thegn_core::hooks` with typed events, scopes, string/object entries,
  event defaults, timeout/wait validation, failure modes, normalized specs,
  ordered global → workspace → repo accumulation, and pure context env
  projection.
- Added global and workspace `[hooks]` config plus typed repo overlay hooks.
- Folded global and repo legacy `sandbox.prepare` into the head of
  `post_create`; repo prepare/hooks share one canonical `hooks.post_create`
  trust request.
- Added per-event repo trust requests using existing `Approvals`/
  `GatedRequest` canonicalization. Pending repo hooks are omitted and approved
  repo hooks are warn-only.
- Documented the new config surface and pinned all six structured hook lists in
  the env-overlay ratchet.

## Verification

- `just quick thegn-core` — passed.
- `cargo nextest run -p thegn-core hooks` — 8 passed.
- `cargo nextest run -p thegn-core --test env_overlay_coverage` — 2 passed.
- `cargo nextest run -p thegn-core --test config_example` — 2 passed.

## Unverified

- The literal `cargo nextest run -p thegn-core env_overlay_coverage` form
  selected no tests because the filter matches test names, not integration-test
  binary names; the equivalent targeted `--test` invocation passed.
- Checks required `XDG_RUNTIME_DIR=/tmp/thegn-runtime-the19` because the
  default runtime directory is read-only here, and `RUSTC_WRAPPER=` because
  the repository sccache socket is unavailable in this sandbox.
- Host runner/orchestration and full-workspace gates belong to later chunks and
  were not run.
