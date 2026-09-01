# Chunk 1 — core preview policy, detection grammar, and configuration

Commit subject (exact): `feat(the-13): add preview core policy and config`

## Files touched

- `crates/thegn-core/src/config.rs` — register the sibling module and
  `Config::preview`, `ConfigOverlay`/apply wiring, and env overlay wiring only;
  do not grow it with preview logic.
- `crates/thegn-core/src/lib.rs` — register the new `config_preview` and
  `preview` modules alongside the existing split config/core modules.
- `crates/thegn-core/src/config_preview.rs` — new `[preview]` schema/defaults,
  validated/clamped policy, and config unit tests.
- `crates/thegn-core/src/preview.rs` — new substrate-free port-hint parser,
  package-script parser, deterministic source merge, target/status values, and
  localhost/redirect policy with unit tests.
- `config/config.toml.example` — document every `[preview]` key, defaults,
  env names, the localhost-only fetch boundary, and that `ports` never launches
  a process.
- `test/env-overlay-ratchet.txt` — regenerate/verify against the new
  `THEGN_PREVIEW_*` overlays; no new debt line is allowed for the five keys.
- `crates/thegn-core/tests/env_overlay_coverage.rs` — extend focused coverage
  only if the existing generic test cannot exercise the five new knobs.

## Approach

Keep all detection and policy pure in `thegn-core`; no `reqwest`, tokio,
termwiz, filesystem reads, process execution, package-manager code, or sandbox
types may enter this module. Parse bounded text supplied by the host. Strip
ANSI/control sequences before matching, accept only loopback/localhost forms,
and reject arbitrary external log URLs. Parse only explicit `--port`, `-p`, and
`PORT=` values from known `dev`/`start` package scripts; do not execute scripts
or infer a broad framework default.

Make `preview.fetch_timeout_ms`, `preview.max_body_bytes`,
`preview.allow_external_urls`, `preview.enabled`, and `preview.ports` all
environment-overridable so the env ratchet remains shrink-only without adding
new unclassified debt. Add pure tests for precedence, duplicates, malformed
ports, ANSI output, package JSON edge cases, loopback authorities, external
opt-in, and redirect revalidation.

Do not add `[browser]`, snapshot-provider kinds, profile paths, cookie import,
or a polling interval. Do not change the existing `browser.drive` contract.

## Overlap/dependency

This chunk is file-disjoint from chunks 2 and 3 and must land first: chunks 2
and 3 consume its `PreviewConfig`, parser, policy, and control-domain values.
It has no dependency on THE-11 implementation, but the later drawer adapter
does depend on the THE-11 registry context hook described in the architecture.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core preview`
- `cargo nextest run -p thegn-core env_overlay`
- `cargo nextest run -p thegn-core config_example`

Use a temporary `XDG_STATE_HOME` for any test/helper that opens the state DB.
Do not run `just test`, `just ci`, a full workspace build, or E2E.

## Done criteria

- The new core module has no substrate dependency and its pure tests cover the
  accepted detection/configuration grammar and security policy.
- The config example, schema, `--set`, and `THEGN_PREVIEW_*` overlays agree;
  `test/env-overlay-ratchet.txt` has no unowned preview key.
- The existing forwarding config and `browser.drive` stub remain behaviorally
  unchanged.
- The coder commits exactly with subject:
  `feat(the-13): add preview core policy and config`
