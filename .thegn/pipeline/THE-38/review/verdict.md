# THE-38 security/test/bug review

PASS

The full `git diff main...HEAD` was reviewed after merging `main`. No remaining
security, correctness, or test-gating findings block the merge queue.

Review fixes committed in `16d497fd`:

- JSON/YAML explicit `null` now correctly validates against optional schema
  branches; regression coverage was added.
- Repository overlay discovery rejects symlinks and non-regular files,
  retains broken/unreadable candidates for diagnostics, and does not read
  lower-priority shadowed files; regression coverage covers symlink and
  shadowing behavior.

The metrics/config-health path remains warning-only and does not execute
repo-provided commands. Existing trust clamping and git argument construction
remain intact. No database or event-loop changes were introduced.

Checks passed:

- `just quick thegn-core`
- `just quick thegn-host`
- Core focused land-gate tests: 68 passed
- Host focused land-gate tests: 169 passed
- `cargo nextest run -p thegn-svc --test control_schema`: 1 passed
- Environment overlay, Home Manager drift, and generated-example ratchets
- `cargo clippy -p thegn-core --tests -- -D warnings`
- `cargo clippy -p thegn-host --tests -- -D warnings`
- `nix develop --command treefmt --ci`
- `nix develop --command openspec validate --all --strict`: 170 passed
- `git diff --check main...HEAD`

The architect-review unverified items (full-workspace test/CI, coverage,
e2e, and unavailable auxiliary ratchet/graph/dispatch checks) were not run by
this scoped review. No migration or live-state database invocation was made.

## Snapshots

None; no frame-affecting changes.
