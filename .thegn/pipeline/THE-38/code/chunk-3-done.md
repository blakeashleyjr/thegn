# THE-38 chunk 3 completion

Synchronized the architecture, user help, and in-flight OpenSpec documents
with the landed configuration-audit implementation.

## Changes

- Documented trusted TOML-only main/profile layers and value-only `THEGN_*` /
  `--set` overlays.
- Documented the untrusted repo-overlay exception, TOML > YAML > YML > JSON
  precedence, path-only shadow warnings, and the complete repo table set.
- Documented tolerant loading, strict all-layer validation, effective-path
  diagnostics, doctor health, and example-value reference semantics.
- Reconciled the proposal, design, task checklist, and config delta with the
  actual `config_repo` and `config_health` APIs.
- Preserved the curated example, canonical config spec, ratchets, control
  schema, and unrelated help pages.

## Verification

- `just quick thegn-core` — passed (with `RUSTC_WRAPPER=` because sccache is
  unavailable in this environment).
- `cargo nextest run -p thegn-core config_reference` — 1 passed, 3615 skipped.
- `just quick thegn-host` — passed (with `RUSTC_WRAPPER=`).
- `cargo nextest run -p thegn-host help` — 75 passed, 2599 skipped.
- `just openspec-validate` — 171 passed, 0 failed.
- `git diff --check` — passed.

## Unverified

Per the chunk policy, no full-workspace `just test`, `just ci`, coverage, or
e2e run was performed. The initial sandbox invocations also could not use the
read-only Nix cache/runtime or sccache; the scoped checks passed after using a
writable temporary runtime directory and disabling the unavailable wrapper.

## Commits

- `342fd84a docs(the-38): clarify configuration format docs`
- Final synchronization commit follows this record.
