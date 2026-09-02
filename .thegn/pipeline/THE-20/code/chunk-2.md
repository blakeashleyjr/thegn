# Chunk 2 — Host seeding, CLI, doctor, catalog, and control schema

## Scope

Consume chunk 1 and implement the host filesystem adapter plus the public
surface. This is the only chunk allowed to touch worktree files at runtime.

## Exact files touched

- `crates/thegn-host/src/skill_seed.rs` (new)
- `crates/thegn-host/src/mq_assets.rs` (delete after its command assets/tests are migrated)
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/cmd/mod.rs`
- `crates/thegn-host/src/cmd/skills.rs` (new)
- `crates/thegn-host/src/cmd/skills_doctor.rs` (new)
- `crates/thegn-host/src/cmd/doctor.rs`
- `crates/thegn-host/src/cmd/session.rs`
- `crates/thegn-host/src/wizard.rs`
- `crates/thegn-host/src/cmd/wt.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/cli_help.rs`
- `crates/thegn-host/src/complete.rs`
- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/completion/catalog.rs`
- `crates/thegn-core/src/completion/sources.rs`
- `crates/thegn-svc/src/control/mod.rs`
- `crates/thegn-svc/src/control/http.rs`
- `crates/thegn-svc/src/control/routes.rs`
- `crates/thegn-svc/src/control/tests.rs`
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-svc/tests/control_schema.rs`
- `docs/api/control-v1.json` (generated additive snapshot)
- `test/completion-slot-ratchet.txt` (generated only when the slot ratchet changes)

Do not edit authored documentation pages or `test/help-*-ratchet.txt` here;
chunk 3 owns those files.

## Approach

1. Replace `mq_assets.rs` with `skill_seed.rs` as the sole host adapter. Move
   the two MQ command assets there, preserve their paths/gates, and route all
   existing wizard, `wt new`, and persisted-worktree call sites through the
   new API. Use the existing off-loop/background mechanism for startup and TUI
   work; explicit CLI seeding may run synchronously. Never introduce a timer or
   render-plan state.
2. Discover only immediate configured user skill packages, merge them with the
   embedded registry using built-ins-first duplicate policy, resolve configured
   harness ids, and map through `Harness::skill_layout`. Survey target files
   before applying the core plan. Apply bounded writes/removals atomically as
   the existing host conventions permit, report per-file conflicts, and
   preserve every unmarked or changed-managed file. Never use a path/name from
   frontmatter before core validation.
3. Add `skills list/show/seed` with stable text and JSON output. `show` is
   read-only; `seed` targets the current or explicit worktree and all configured
   harnesses. Explicit failures are visible; background failures remain
   best-effort diagnostics.
4. Add the read-only doctor report as a sibling module and make text/JSON share
   one state model. It must not repair files or scan arbitrary home trees.
5. Add `SkillsList`/`SkillsSeed`, centralized scopes, catalog rows, CLI coverage,
   and a `Skill` completion source. Classify `skills seed --worktree` as
   structural. Keep the capability surface intentionally `skills.list: CLI +
HTTP` and `skills.seed: CLI`.
6. Add `SkillInfo` and `ControlApi::list_skills`, the `GET /v1/skills` route,
   `API_CALLS` row, daemon implementation, fake implementation, and the
   additive control snapshot. The route must list metadata only and must never
   accept a target path or seed request. Run the schema test with the repository
   snapshot-update mechanism, not a built binary or live state DB.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core skills`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core completion`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host skills`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host cli_help`
- `cargo nextest run -p thegn-host complete`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc control_schema`
- `cargo nextest run -p thegn-svc routes`

Run the completion-slot ratchet/update test only when needed and commit its
generated shrink. Do not run `just test`, `just ci`, e2e, migrations, or a
full-workspace compile. If any `thegn` invocation is needed for a smoke check,
set `XDG_STATE_HOME` to a fresh temporary directory first.

## Dependency and overlap

This chunk is serial after chunk 1 because it consumes its APIs. Chunk 3 is
serial after this chunk because its CLI grammar and help prose depend on the
final command names and status vocabulary. The file lists are disjoint from
chunks 1 and 3 except for generated ratchets explicitly listed here; do not
parallelize dependent work.

## Done criteria

- Existing `mq`, `pipeline`, and `supervise` seeds retain content, gate timing,
  Claude paths, and best-effort lifecycle behavior; no second skill registry or
  unconditional `fs::write` remains.
- Claude, Codex, and Pi receive skills only in their native project layouts;
  unsupported harnesses degrade with a diagnostic.
- Repeated seeding is a no-op; unmarked and user-modified managed files survive;
  only proven-unmodified managed files can be removed for exclusion/deprecation.
- `skills list/show/seed`, doctor, catalog coverage, completion coverage,
  route/API mirror, and the committed control schema all pass.
- Commit exactly as: `feat(thegn): seed embedded skills per harness`
