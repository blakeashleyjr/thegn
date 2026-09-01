# THE-26 chunk 2 — debugger gate in doctor JSON

## Scope

Expose the already-existing pure BugStalker platform gate in machine-readable
doctor output. This closes the evidence gap between human doctor output and
`doctor --json`; it does not add debugger adapters.

## Files touched (exact)

- `crates/thegn-host/src/cmd/doctor.rs`

No other files are in scope. Do not add `[[debug.adapters]]`, `--adapter`, a
vendor probe, a resolver/install action, a config key, or a capability-catalog
row.

## Approach

1. Extend the existing `managed_tools_json` BugStalker object with stable
   platform-gate data, such as `platform_supported` and a nullable
   `platform_note` (use the repository’s established JSON naming convention).
2. Obtain the values from the same pure `(os, arch)` gate/reason used by the
   human managed-tool report and `thegn-core/src/debug.rs`; do not duplicate a
   second vendor/platform policy in the JSON formatter.
3. Add a focused unit test beside the existing managed-tools JSON test. Assert
   both supported and unsupported gate behavior using pure inputs or the
   repository’s existing gate test seam, plus the existing override tier/path
   behavior. Keep the JSON addition backward-compatible for consumers that
   ignore unknown fields.

## Overlap and dependency

Independent of chunks 1 and 3; no shared files or ordering dependency. The
Lead may run this chunk in parallel with both other chunks.

## Tests to run

From the worktree:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host doctor`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e. If a
manual `thegn doctor --json` invocation is useful, run it only with
`XDG_STATE_HOME="$(mktemp -d)"`.

## Done criteria

- `thegn doctor --json` reports BugStalker’s platform support and an
  unsupported reason using the same pure gate as the human doctor report.
- Existing managed-tool fields and override behavior remain intact.
- No external process is launched by the JSON formatter; no adapter registry,
  config/env/CLI key, catalog row, or migration is introduced.
- Env-overlay, completion-slot, control-schema, and all help ratchets are
  verified unchanged.
- The exact commit subject is:

  `fix(the-26): expose debugger gate in doctor JSON`
