# Chunk 1 — core validation substrate and generated-reference drift

## Scope

Implement the substrate-free, unit-tested core work for THE-38. Keep trusted
configuration TOML-only. Add format-neutral validation for the already
supported repo-local TOML/YAML/JSON documents, shadowed-candidate diagnostics,
and a real generated-reference coverage gate. Do not add a new trusted reader,
format selector, config key, or default-value heuristic.

## Files touched

- `crates/thegn-core/src/config_repo.rs` — new sibling module for repo format
  enum, candidate discovery, `RepoConfigFile` ownership/re-export, parsing,
  shadow detection, and the public pure repo-overlay validation entry point.
- `crates/thegn-core/src/lib.rs` — register the new module.
- `crates/thegn-core/src/config.rs` — remove or delegate the existing repo
  candidate/parser implementation without changing precedence, trust clamps,
  metrics refusal, or tolerant-load behavior; preserve compatibility exports.
- `crates/thegn-core/src/config_validate.rs` — share the schema walk between
  `Config` TOML validation and repo-overlay values; add dotted-key type
  diagnostics while preserving unknown-key hints and legacy-key behavior.
- `crates/thegn-core/src/config_tests.rs` — unit tests for all supported repo
  formats, syntax/unknown/type errors, shadow candidates, and unchanged
  effective overlay behavior.
- `crates/thegn-core/src/help/config_ref.rs` — change the generated-page claim
  from code defaults to example values and add schema-derived key coverage for
  the generated reference.
- `crates/thegn-core/tests/config_example.rs` — share or extend the example
  scanner/test helper so generated-reference coverage is tied to the same
  schema/example key set.

## Approach

1. Extract the current `.thegn.toml`, `.thegn.yaml`, `.thegn.yml`,
   `.thegn.json` candidate order into `config_repo.rs`. The first existing
   candidate remains the winner. Existing equivalent-format behavior and
   repo trust filtering remain unchanged.
2. Define a public `OverlayFormat`/candidate API that lets host code validate
   the selected repo file without reaching into `pub(crate)` internals. The
   validation API accepts the body and format, parses into one neutral value,
   and applies the same schema walk used for `Config` against the actual
   `RepoConfigFile` schema.
3. Make the shared walk report syntax, unknown-key, and type failures with a
   dotted path. Keep dynamic map names accepted, retain nearest-key hints, and
   avoid duplicate reports for legacy sections. Keep `validate_str` as the
   existing Config entry point.
4. Have candidate discovery return all existing paths. When more than one is
   present, emit one path-only warning naming the winner and ignored files;
   repeated effective-field reads during one load must not spam the warning.
   Never include repo file contents in diagnostics.
5. Add tests for valid equivalent TOML/YAML/JSON overlays; syntax, unknown
   top-level, nested typo-with-hint, and type errors in each format; a shadowed
   candidate; metrics detection/refusal; and the existing all-fields merge.
6. Make `config-reference` say it contains every documented key and its
   example value. Add a test that obtains the expected key set from the schema
   and shipped example, then verifies the generated page contains every key.
   Do not compare commented example values against `Config::default()`.

The new modules are required to keep `config.rs` and `config_validate.rs` from
growing into additional god files. No file I/O or terminal logging belongs in
the validation walk itself; candidate discovery may expose paths, while host
code owns user-facing aggregation.

## Overlap and dependency

No file overlap with chunks 2 or 3. Chunk 2 depends on the public core API
created here and must run serially after this chunk; chunk 3 is documentation
and OpenSpec only and is file-disjoint. Do not modify host files in this chunk.

## Tests to run

Run only scoped checks:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core config_validate`
- `cargo nextest run -p thegn-core repo_overlay`
- `cargo nextest run -p thegn-core config_example`
- `cargo nextest run -p thegn-core config_reference`

Also confirm the existing ratchet-focused tests remain green when included by
the scoped core quick check:

- `cargo nextest run -p thegn-core env_overlay_coverage`
- `cargo nextest run -p thegn-core hm_module_drift`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e.

## Done criteria

- `Config`, external profile files, keymaps, themes, `--set`, and env overlays
  still have exactly their existing TOML/value semantics; no JSON/YAML trusted
  reader is introduced.
- Repo candidates still resolve TOML > YAML > YML > JSON, and all existing
  security/clamp/metrics behavior is preserved.
- Core tests prove valid and invalid behavior in TOML, YAML, and JSON and show
  diagnostics naming the relevant dotted key; shadow warnings name paths only.
- The shipped example still passes schema coverage, deserialization, and clean
  validation; the generated reference fails if a documented schema key is
  dropped and no longer promises exact code defaults.
- `env_overlay_coverage`, `hm_module_drift`, and the config enum-count ratchet
  require no new entries because no config key was added.
- Commit with exactly:

  `feat(the-38): validate all config documents`
