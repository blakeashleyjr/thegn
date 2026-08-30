# THE-29 chunk 1 — done

**Commit subject:** `feat(the-29): add pure harness fork policy and lineage cache`

## Implemented

- Added the optional `HarnessCaps::FORK` / `Harness::fork_command` seam and
  caps⇔operation coverage. Claude uses its native resume-plus-fork command;
  Codex uses its native `fork` command. Aider, Antigravity, and Pi remain
  reserved/unsupported.
- Added substrate-free `thegn_core::session_fork` contracts for raw daemon
  recipes, recorded harness sessions, bounded validation, deterministic fork
  plans, placement intent, identity-environment overwrite, and credential-free
  `ForkRecord` lineage metadata.
- Added the v62 `session_forks` store seam and migration/verification ladder.
  The cache stores only child/source lineage, optional harness/worktree, and
  creation time; argv, environment, prompts, transcripts, credentials, and raw
  recipes are not persisted.
- Updated `docs/extending/harness.md` with the fork capability and no-config-key
  behavior. No config or env-overlay ratchet changes were needed.

## Tests

- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp just quick thegn-core` — passed.
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp cargo nextest run -p thegn-core harness` —
  passed (32 tests).
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp cargo nextest run -p thegn-core session_fork` —
  passed (8 tests).
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp cargo nextest run -p thegn-core db_migrate` —
  passed (15 tests).

## Unverified

- Full workspace gates (`just test`, `just ci`, coverage, and e2e) were not run
  per the chunk dev-loop policy.
- No live `thegn` invocation or migration against the normal state directory
  was run. The first `just quick` attempt was blocked by the environment's
  read-only `/run/user/1000`; rerunning with runtime temp state in `/tmp` and
  the unavailable sccache wrapper disabled passed.
