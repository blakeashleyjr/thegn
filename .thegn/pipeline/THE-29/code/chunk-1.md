# Chunk 1 — core fork policy and contracts

## Scope

Implement the substrate-free contract and policy layer first. This chunk is
file-disjoint from chunks 2 and 3 and must land before them because their wire
and host code consumes these types and catalog rows.

## Files touched

- `crates/thegn-core/src/harness.rs`
- `crates/thegn-core/src/session_fork.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/store/mod.rs`
- `crates/thegn-core/src/store/session_fork.rs` (new)
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-core/src/db.rs`
- `docs/extending/harness.md`

Do not add an artificial completion-ratchet line when the new arguments are
already classified by the catalog; update the listed ratchet only if the
repository’s generated ratchet requires it.

## Approach

1. Add `HarnessCaps::FORK` and `Harness::fork_command(native_session_id)` as
   an optional operation. Extend the caps⇔ops test for every registered
   harness. Keep exact vendor command syntax inside each harness implementation:
   implement only behavior verified on this branch; leave unsupported harnesses
   as `None` with the capability reported as reserved. Reuse the existing
   session-id validator and quoting tests.
2. Add `session_fork.rs` with typed source/options, validation, raw-recipe and
   native-harness `ForkPlan`s, and `ForkRecord`. Core may manipulate strings,
   vectors, and environment data as pure values, but may not import tokio,
   PTY, HTTP, git, filesystem, or vendor session-file types. Unit-test all
   policy branches and ensure serialization of a record cannot contain env,
   argv, prompt, transcript, or credential fields.
3. Add the v62 credential-free `session_forks` cache through the store seam and
   migration ladder. Store only source/child lineage, harness identifier,
   worktree, and timestamps. Add round-trip and v61→v62 migration tests; cache
   writes remain best-effort at host edges.
4. Update the harness extension documentation. Do not add a config key; leave
   `config/config.toml.example` and the env-overlay ratchet unchanged unless an
   existing comment is genuinely corrected. Chunk 2 checks the env-overlay
   ratchet together with the control/completion contract changes.

## Overlap/dependency

No file overlaps chunk 2 or 3. If `AdoptIntent` must gain the `tab` field,
chunk 3 owns that file and chunk 1 exposes only a core placement enum/type in
`session_fork.rs`. Chunk 2 depends on this commit’s harness/policy/cache
contract. Chunk 3 depends on both chunks 1 and 2. Run serially in that order;
the Lead must not parallelize dependent chunks.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core harness`
- `cargo nextest run -p thegn-core session_fork`
- `cargo nextest run -p thegn-core db_migrate`

Do not run `just test`, `just ci`, a workspace compile, e2e, or a binary
against the normal state directory.

## Done criteria

- Core remains substrate-free and the new module avoids growth of `agent_task`
  or `db.rs` beyond wiring.
- `FORK` is caps⇔optional-op for every harness; unsupported implementations
  are surfaced as reserved/unsupported and never guessed.
- Fork policy has deterministic unit tests for raw replay, native harness
  support, invalid/dead/unsupported sources, and credential-free records.
- v62 migration/store tests pass and no recipe or scrollback contents are
  persisted.
- Harness documentation is updated here; the env-overlay check and
  catalog/MCP/completion ratchets land atomically with chunk 2.
- Commit exactly as: `feat(the-29): add pure harness fork policy and lineage cache`.
