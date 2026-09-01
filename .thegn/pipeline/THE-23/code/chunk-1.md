# THE-23 chunk 1 — core devcontainer contract

## Files touched

- `crates/thegn-core/src/devcontainer.rs`
- `crates/thegn-core/src/devcontainer_select.rs` (new)
- `crates/thegn-core/src/devcontainer_inventory.rs` (new)
- `crates/thegn-core/src/devcontainer_overlay.rs`
- `crates/thegn-core/src/sandbox_cpucap.rs`
- `crates/thegn-core/src/envplan.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_sandbox.rs`
- `crates/thegn-core/src/config_resolve.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/tests/fixtures/devcontainer/primary.json`
- `crates/thegn-core/tests/fixtures/devcontainer/jsonc.json`
- `crates/thegn-core/tests/fixtures/devcontainer/variant.json`
- `config/config.toml.example`
- `test/env-overlay-ratchet.txt`

Do not touch host files, `docs/api/control-v1.json`, completion-slot ratchets,
or help ratchets in this chunk.

## Approach

Extract selection and field classification into sibling modules so the parser
and config files do not grow into new god files. Keep all code pure and
substrate-free.

Implement deterministic primary/variant discovery, explicit selector support,
ambiguity and read/parse error results, JSONC fixture parsing, and the stable
`${devcontainerId}` substitution. Add a substitution report and an allowlist
predicate so `${localEnv:NAME}` is empty plus reported when `NAME` is not in
the effective `sandbox.env_passthrough` list. Preserve existing normalized
fields and existing feature declaration/override ordering.

Add the exhaustive recognized-field disposition table. Apply the requested
subset and existing tested mappings; refuse isolation-weakening keys without a
trust request; report reserved/editor-only/unknown keys. Make the overlay use
that shared inventory and expose a pure backend-honourability result. Preserve
trusted pinned backend/profile/network/image/build precedence and additive list
semantics.

Add `SandboxConfig.devcontainer` with `auto`/`off`, its repo-overlay and
environment-layer plumbing, and the top-level repo selector. `off` must be a
decision available to the host before parse. Update `envplan` to use the same
candidate rules as the parser. Add the config example comments for both keys.

Expose one narrow, pure CPU-cap argv wrapper for provider-backed pane launches
in `sandbox_cpucap.rs`; it must share the existing mechanism rather than let a
provider invent its own limit path.

Tests must cover JSONC comments/trailing commas, malformed/read failures,
primary precedence, explicit variant selection and ambiguity, fixture field
classification, refused fields, stable/distinct IDs, allowlisted versus
blocked localEnv, user precedence, config enum round-trip, overlay
exhaustiveness, and env overlay behavior.

## Dependency / overlap

This chunk is first and has no dependency on other chunks. Chunk 2 depends on
the public core selection, inspection, overlay, config, and status contracts
defined here. Chunk 3 depends on the final names and behavior but touches none
of these files.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core devcontainer`
- `cargo nextest run -p thegn-core config::tests::env_overlay`

Run the focused config/overlay filters that match the repository’s actual test
names if module-qualified filtering differs. Do not run the Tier 2
`devcontainer_e2e`, any migration, a built binary, or a full-workspace gate.

Ratchets in this commit: add the new `sandbox.devcontainer` entry to
`test/env-overlay-ratchet.txt` and its core env-overlay test. Run completion
slot, control-schema, and help ratchet checks as read-only checks; they must be
byte-identical because no CLI argument, control operation, action, or panel
context was added.

## Done criteria

- Core has one deterministic selection/classification source of truth and no
  subprocess, network, tokio, PTY, or vendor dependency.
- No recognized security-sensitive field can reach a sandbox provider, even if
  other categories are approved.
- No blocked host variable value is copied into the parsed container model or
  warning output.
- All listed focused tests pass, config example and env-overlay ratchet are in
  the same commit, and existing tests compile without broadening the build.
- Commit exactly as: `feat(the-23): define core devcontainer contract`
