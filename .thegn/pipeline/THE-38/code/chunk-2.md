# Chunk 2 — host validation, config context, doctor health

## Scope

Expose the core validation contract through the host CLI. `thegn config
validate` must inspect every locatable configuration layer, `config get/set`
must provide file context for failures, and `thegn doctor` must report the
same validation counts without duplicating policy. Add the completion catalog
entry required by the new `--repo` argument.

## Files touched

- `crates/thegn-host/src/cmd/config.rs` — add `Validate --repo <path>`, call
  the shared layer collector, prefix diagnostics with owning paths, and add
  effective config-path context to `get`/`set` failures while retaining typed
  JSON and atomic rollback.
- `crates/thegn-host/src/cmd/config_health.rs` — new host-edge collector for
  main TOML, active profile TOML, and selected repo overlay paths/results;
  aggregate problem counts and render-neutral health data for config and
  doctor.
- `crates/thegn-host/src/cmd/mod.rs` — register `config_health`.
- `crates/thegn-host/src/cmd/doctor.rs` — consume the collector in text and
  JSON output; do not reimplement validation.
- `crates/thegn-host/src/cmd/bundle.rs` — update the existing doctor JSON call
  if its signature/context changes, preserving bundle output.
- `crates/thegn-host/src/main.rs` — pass the resolved `config_path` and repo
  context into doctor/config dispatch so explicit `--config` is reported.
- `crates/thegn-core/src/completion/catalog.rs` — register the value-taking
  `config validate --repo` slot as `SourceKind::Structural` so clap owns path
  completion.

## Approach

1. Extend `Action::Validate` with an optional `--repo` path. The global
   `--config` path remains the main document path; its extension does not
   change TOML parsing.
2. Implement `config_health.rs` as a thin host-side file/path adapter over
   chunk 1's core APIs. Validate, in order: the selected main TOML, the active
   external profile `profiles/<name>/config.toml` when present, and the
   selected repo candidate from cwd or `--repo`. Missing optional files are
   silent. Report every finding as `<path>: <key/path>: <diagnostic>` and
   return a non-zero result if any layer has a problem.
3. Use the active profile already selected by core; do not validate inactive
   profile files. Validate profile files as Config-shaped TOML overlays and
   repo files as `RepoConfigFile` in their discovered format. Reuse candidate
   shadow warnings from core, and do not print raw repo contents.
4. Keep `config get`'s output value-compatible, especially `--json`. For
   unknown keys and `set` validation/parse failures, include the effective
   config path as context and the requested dotted key. Do not invent
   provenance for a key merely because the effective config has a path.
5. Feed the same health result into `doctor`: JSON gets a stable
   `config_health` object containing main/profile/repo layer paths and problem
   counts plus `thegn config validate` as the detail command; text gets one
   concise equivalent line. Doctor remains diagnostic-only and keeps its
   existing exit behavior.
6. Add `slot("config validate", "repo", SourceKind::Structural)` to the single
   completion catalog. Do not edit `test/completion-slot-ratchet.txt`; the
   catalog entry is the ratchet-compliant source of truth for path completion.

Keep I/O in this one-shot CLI edge. Do not add reads to the compositor or
change any control capability/schema. Preserve the existing synchronous-doctor
exception and all provider/sandbox diagnostics.

## Overlap and dependency

No file overlap with chunks 1 or 3. This chunk has an API dependency on chunk 1
and must be implemented/tested serially after it, although its files are
disjoint. Chunk 3 is file-disjoint and may run in parallel. If the core API is
renamed, update this chunk against the landed public signature rather than
reintroducing parsing logic in host code.

## Tests to run

Run only scoped checks:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host config`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-core completion`
- `cargo nextest run -p thegn-core config_validate`

The quick checks must include the completion-slot, help, and control-schema
ratchet tests. Confirm explicitly that no control-schema snapshot changes and
that the env-overlay/home-manager/config-enum ratchets remain unchanged from
chunk 1. Do not run `just test`, `just ci`, a full-workspace compile, or e2e.

## Done criteria

- `thegn config validate` has no nonexistent `--strict` flag, validates the
  main file, active profile overlay, and selected repo overlay, prefixes every
  finding with its file, and exits non-zero for any finding.
- Unknown keys retain nearest-key hints; type errors name the dotted key and
  file; absent optional layers are not errors.
- `config get/set` diagnostics include the effective config path while typed
  JSON and atomic rollback behavior remain intact.
- Doctor text and JSON report config-health paths/counts and point to
  `thegn config validate`; doctor does not duplicate validator policy or alter
  its existing exit policy. Bundle JSON remains compatible.
- The `--repo` slot is present in the capability/completion catalog as
  structural path completion; no completion allowlist, env-overlay ratchet,
  help ratchet, control schema, or config key is added unnecessarily.
- Commit with exactly:

  `feat(the-38): surface config health in CLI diagnostics`
