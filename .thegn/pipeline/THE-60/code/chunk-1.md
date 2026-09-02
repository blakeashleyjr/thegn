# Chunk 1 — core detection and activation seam

## Scope

Create the substrate-free generic toolchain activation model and make mise
detection/configuration complete. This chunk is first and is a prerequisite for
chunks 2 and 3. It must not add host subprocesses, terminal types, tokio,
control routes, or UI actions.

## Files touched

- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/envplan.rs`
- `crates/thegn-core/src/toolchain.rs`
- `crates/thegn-core/src/toolchain_activation.rs` (new)
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_validate.rs`
- `config/config.toml.example`
- `test/env-overlay-ratchet.txt`

Do not touch `crates/thegn-core/src/bundle.rs`; call its public credential
filter from the new module. Do not touch host or service files.

## Approach

1. Add the normalized detected mise config/pin set to `EnvRequirements` while
   preserving `tool_versions` and the existing Nix-first tier order. Extend
   both `detect()` and `DETECT_PROBE_SCRIPT` together, with deterministic
   `conf.d/*.toml` ordering and safe `MISE_ENV` parsing.
2. Add `[toolchain.mise].inject` with `auto|shims|env|off`, default `auto`, via
   the existing config enum machinery. Register it in config validation and
   document every key and the security rationale in the example.
3. Add `toolchain_activation.rs` with the object-safe provider seam, `Ready` /
   `Unavailable` / `Reserved` answers, cache/trust identity, and pure activation
   composition. Reuse `bundle::is_credential_key`; enforce bundle > devshell >
   mise > base PATH and fill-only env semantics.
4. Add unit tests for detection/probe parity, all listed files, malformed
   remote output, lock/config edit invalidation, trust canonicalization,
   `Reserved`, `inject` modes, PATH ordering, fill-only env, and credential
   filtering. Keep all tests filesystem-only or fake-provider tests.
5. Add `toolchain.mise.inject` to the env-overlay ratchet as intentionally
   pinned beside the existing toolchain keys. Do not add an ambient env override
   for the trust/activation policy.

## Dependency/overlap

Serial prerequisite for chunk 2; chunk 2 consumes these public core types.
Chunk 3 depends on chunk 2's host operation. Files are disjoint from chunks 2
and 3.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core envplan`
- `cargo nextest run -p thegn-core toolchain_activation`
- `cargo nextest run -p thegn-core config`
- `cargo nextest run -p thegn-core env_overlay`

Do not run a full workspace build, `just test`, `just ci`, or e2e.

## Done criteria

- Core contains no `mise` process invocation and remains substrate-free.
- Local and remote detection produce the same normalized file set.
- Nix provisioning precedence remains unchanged; activation precedence is
  deterministic and unit-tested.
- Trust identity contains no env values or secrets and changes on config/lock
  edits.
- The config example and env-overlay ratchet are updated in this commit.
- Commit exactly as: `feat(core): add generic toolchain activation seam`
