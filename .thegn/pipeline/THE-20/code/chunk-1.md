# Chunk 1 — Core registry, config, and harness metadata

## Scope

Build the substrate-free data/model seam that later chunks consume. This
chunk must not add CLI commands, filesystem reads, host seeding, or control
routes.

## Exact files touched

- `crates/thegn-core/src/skills.rs` (new)
- `crates/thegn-core/src/config_skills.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/src/harness.rs`
- `config/config.toml.example`
- `extensions/skills/mq/SKILL.md`
- `extensions/skills/pipeline/SKILL.md`
- `extensions/skills/supervise/SKILL.md`
- `test/env-overlay-ratchet.txt` (only if the repository ratchet generator produces a delta)

Do not edit host, service, CLI, help, completion, capability, or control-route
files in this chunk.

## Approach

1. Add the `SkillsConfig` schema with `enabled = true`, `user_dirs = []`, and
   `exclude = []`; flatten it into `Config` with default/serde behavior and
   expose all three shallow values through `ConfigOverlay`. Validate names and
   directory-list syntax at the config boundary while leaving filesystem
   existence to the host edge. Add the env-overlay test cases and run the
   env-overlay ratchet; do not create a new unclassified env knob.
2. Add `thegn-core::skills` with the bounded frontmatter parser, typed gates and
   phases, path-safe names, deterministic registry, embedded manifest, and
   pure marker/hash/seed-plan types. Use existing dependencies only. The plan
   must distinguish absent, current, changed-managed, unmarked, excluded, and
   deprecated files without reading or writing paths.
3. Add the harness skill-layout method and return only relative project roots
   from the Claude, Codex, and Pi implementations. Unsupported harnesses return
   `None`; no generic vendor path switch is allowed.
4. Re-express the three existing skill documents with equivalent body and gate
   semantics and the new frontmatter. Keep `tui-check` out of the manifest.
5. Put all new logic in the new modules; keep `config.rs` as field/layer glue.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core skills`
- `cargo nextest run -p thegn-core config`
- `cargo nextest run -p thegn-core env_overlay`

Run the env-overlay ratchet/update command used by this checkout if the
coverage test requests it, and commit only the generated ratchet result. Do
not run `just test`, `just ci`, an e2e test, or a full-workspace compile.

## Dependency and overlap

Chunk 2 depends on the public `skills`, `SkillsConfig`, and harness-layout
interfaces from this chunk. Chunk 3 depends transitively on both later CLI and
core interfaces. File ownership is otherwise disjoint: the Lead runs this
chunk first and does not parallelize it with chunks 2 or 3.

## Done criteria

- Core has no filesystem, tokio, clap, termwiz, process, or vendor SDK
  dependency for skills.
- All three built-ins parse and preserve the current gate behavior; `tui-check`
  is absent.
- The pure planner proves idempotence and never schedules overwrite/delete for
  unmarked or hash-mismatched content.
- Config defaults preserve existing seeding and every new key is present in
  `config/config.toml.example`; env-overlay coverage is green.
- Core unit tests pass with the scoped commands above.
- Commit exactly as: `feat(thegn-core): add embedded skills registry`
