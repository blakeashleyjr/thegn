# THE-35 chunk 3 completion

Implemented the final documentation and specification chunk:

- Documented the complete `[notifications.sound]` contract in
  `config/config.toml.example`, including the bell default, all keys,
  `SoundRef` values, trusted paths/overlays, provider capabilities, and
  terminal-bell fallback behavior.
- Added a comprehensive Sound effects section to
  `docs/help/notifications.md`, covering the event catalog, gates, pack/file
  resolution, doctor diagnostics, bounded best-effort playback, and the lack
  of a sound control action.
- Reconciled the openspec proposal, design, tasks, and notifications delta
  with the compiled implementation: explicit `pack:<name>` references, no
  synthesized family or bundled audio, fixed-argv platform providers,
  bounded off-loop playback, live-attention edges, trusted overlays, and no
  database/control/capability additions.
- Recorded the ignored-result ratchet limitation without changing sibling
  chunk-2 implementation or adding ratchet debt.

Commits:

- `1c9361f8 docs(the-35): add sound configuration reference`
- `ddb480f5 docs(the-35): reconcile sound openspec contract`
- final commit: `docs(the-35): document configurable sound effects and ratchets`

## Verification

- `JUST_TEMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-core` — passed.
- `JUST_TEMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core --test config_example` — 2 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core env_overlay` — 8 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host help` — 75 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host doctor` — 19 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-svc --test control_schema` — 1 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host platform_cfg` — 1 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host completion_slots_are_bound_or_pinned` — 1 passed.
- `git diff --check` — passed.

## Unverified

- The exact `cargo nextest run -p thegn-core config_example` filter matched
  no tests in this checkout; the explicit `--test config_example` invocation
  passed both integration tests.
- `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates`
  reports the sibling-owned `crates/thegn-host/src/notification_sound.rs`.
  This chunk leaves that implementation and the ratchet allowlist untouched.
- No full-workspace `just test`, `just ci`, coverage, or e2e run was performed,
  per the issue's dev-loop policy.
