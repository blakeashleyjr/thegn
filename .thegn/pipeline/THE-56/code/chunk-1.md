# Chunk 1 — core autopilot policy, config, and durable claim journal

## Dependency and overlap

This chunk is first and is independent of Chunk 2's files, but it has an
external sequencing dependency: rebase after THE-27 and THE-48 have reconciled
their schema migrations. The current branch is v61 and both in-flight designs
use v62, so select the next merged version (expected v63 when both land). Do
not claim or stamp v62 in isolation. Chunk 2 depends on the public config,
policy, and store APIs from this chunk and must run serially afterward.

## Files touched

- `crates/thegn-core/src/autopilot.rs` — new substrate-free state, matching,
  transition, and bounded-summary types plus unit tests.
- `crates/thegn-core/src/config_autopilot.rs` — new serde config, defaults,
  workspace overlay, validation helpers, and unit tests.
- `crates/thegn-core/src/config.rs` — module re-export, `Config` and
  `WorkspaceConfig` fields/defaults/overlay application, and
  `Config::repo_autopilot` resolver.
- `crates/thegn-core/src/config_validate.rs` — validate agent command and the
  closed enum/value policy for autopilot config at global and workspace scope.
- `crates/thegn-core/src/lib.rs` — register the new core modules.
- `crates/thegn-core/src/store/autopilot.rs` — `AutopilotStore` row/claim/read/
  transition trait and DB implementation-facing types.
- `crates/thegn-core/src/store/mod.rs` — register/export the store module.
- `crates/thegn-core/src/db_autopilot.rs` — isolated SQL for claims, bounded
  list/get, transition, dispatch correlation, and PR lookup.
- `crates/thegn-core/src/db.rs` — migration registration and schema version
  history only; preserve the existing file's module boundaries.
- `crates/thegn-core/src/db_migrate.rs` — one additive next-version migration,
  verifier, and migration tests after THE-27/THE-48 are reconciled.
- `config/config.toml.example` — documented `[autopilot]` defaults and the
  trusted `[workspace.<slug>.autopilot]` overlay shape.
- `docs/help/configuration.md` — operator-facing explanation of every new key,
  trust boundary, default-off behavior, and provider-side `me` semantics.
- `test/env-overlay-ratchet.txt` — classify/pin every new config key at global
  and workspace scope according to the existing ratchet convention.

## Approach

1. Keep all policy decisions pure and testable. `matches_issue` receives an
   `Issue`, configured label/status, and the fact that the provider result came
   from `filter_assignee_me`; it does not inspect issue body text or call a
   provider. `assignee` parses only `me` in v1; reject unknown values instead
   of silently broadening pickup.
2. Model a provider-qualified issue key with provider and account, not just a
   display number. Normalize no vendor-specific IDs beyond trimming the stable
   values already supplied by the issue seam.
3. Define legal state transitions and terminal states in `autopilot.rs`.
   Cap stored errors/reasons to the project’s existing bounded-string policy;
   never persist issue bodies, command output, environment values, or tokens.
4. Add the config as a sibling module rather than growing `config.rs` with
   policy logic. The global table defaults to disabled. The workspace overlay
   may refine trusted user config; do not add repo-root overlay fields that can
   enable a supervisor or widen command/sandbox policy.
5. Add `autopilot_runs` through the isolated DB module. Enforce the unique
   provider/account/issue key in SQLite and make `claim` atomic: a concurrent
   insert returns “already claimed,” not an error that can cause a second run.
   Bound active-row counting by repo and non-terminal state. Transitions must
   verify the expected prior state so a stale worker cannot overwrite a newer
   terminal outcome. Store the existing `agent_dispatches.id` after Chunk 2
   creates the queued role/dispatch record; this table is correlation, not a
   replacement dispatch vocabulary.
6. Reconcile the migration ladder with THE-27/THE-48 before finalizing the
   version. Verify fresh DB, upgrade DB, unique claim, transition guard,
   reopen/readback, and bounded list behavior. Existing state remains the
   source of truth; this table is a resumable ledger/cache of the supervisor,
   not a tracker replacement.
7. Update all config/help/env ratchets in this same chunk. Do not introduce a
   prompt-template config: the existing `TaskKind::Issue` prompt and configured
   agent command are the single handoff path.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core autopilot`
- `cargo nextest run -p thegn-core db_migrate`
- `cargo nextest run -p thegn-core config`

These are scoped crate/filter runs only. Do not run `just test`, `just ci`, a
full-workspace compile, e2e, or the binary against the shared state DB.

## Done criteria

- `AutopilotConfig` is default-off, validated, serde-compatible, and resolves
  through `Config::repo_autopilot`; all keys appear in the example, help, and
  env-overlay ratchet.
- Core policy tests prove exact label/status/consent matching, duplicate claim
  rejection, active concurrency bound, legal transitions, terminal guards, and
  bounded error data without importing host/service/vendor code.
- The additive migration is the next reconciled schema version after THE-27/
  THE-48, not a competing v62; fresh and upgrade paths verify the table/index.
- The store exposes an atomic claim and expected-state transition API suitable
  for an off-loop host driver; no network/process/git work enters the core.
- No existing `TaskKind`, pipeline scheduler, provider backend, or PR queue is
  duplicated.
- Commit exactly as:

  `feat(the-56): add autopilot claim policy and journal`
