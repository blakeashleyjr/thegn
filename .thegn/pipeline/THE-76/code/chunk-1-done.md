# THE-76 chunk 1 — completion summary (code stage)

Coder stage, chunk 1 of improve-agent-pipeline-v2. Branch `tg/the-76-pipeline-v2`,
commit `feat(pipeline): stage permissions, artifact paths and run-completion
policy (THE-76)` (code + openspec + this summary in one commit).

## What was implemented

Exactly the chunk spec — `thegn-core` only, plus `config/` and `openspec/`:

1. **`crates/thegn-core/src/pipeline_run.rs` (NEW, pure).** The mechanism half
   of the pipeline; module doc cross-references `config_pipeline.rs`'s
   "structure, not judgment" doctrine and states that nothing here advances a
   stage. No I/O, no subprocess, no tokio, no `Db`.
   - `issue_key` / `artifact_path` — `.thegn/pipeline/<ISSUE>/<stage>/<row>.md`
     with whitelist sanitization (`[A-Za-z0-9._-]`, everything else → `-`,
     runs collapsed, leading/trailing `-`/`.` trimmed, empty → `issue`/`stage`);
     documented as a security boundary.
   - `VerifyFacts` / `VerifyReport` / `verify_report` — no-artifact rows are
     never gated; `ok = exists && tracked` with one reason per miss (exact
     strings from the spec); `dirty` reported, never blocking, and kept out of
     `reasons` so callers can print them verbatim.
   - `WaitTarget` / `WaitSelectError` / `wait_candidates` — waitable =
     `Spawning | Running` with a non-empty session; `--any` in roster order;
     each miss its own error variant; Display in operator language. The doc
     comment carries the is_active-vs-waitable reasoning (WaitingHuman/PrOpen
     must not make `--any` return instantly and forever).
   - `PermsError` / `merge_claude_allow` — pure `.claude/settings.local.json`
     allow-list merge: blank → `{}`, malformed or wrongly-shaped
     (root/permissions/permissions.allow, incl. non-string allow entries)
     refused, never overwritten; unrelated keys survive byte-for-byte in
     value; union with first-occurrence order; idempotent; pretty + trailing
     newline. Doc notes key order follows serde_json's default Map.
   - `with_fresh_registry` — clones `base`, overwrites only
     `agents`/`tools`/`pipeline`; doc explains the narrowness (boot-time
     `--set`/`--config` overrides must survive; only a stage's `agent` name
     goes stale).
   - 21 unit tests, named for properties, covering every public item.
2. **`lib.rs`** — `pub mod pipeline_run;` (alphabetical).
3. **`config_pipeline.rs`** — `PipelineStage.permissions: Vec<String>` with the
   spec's doc comment; `Default` extended; `validate_pipeline` now rejects an
   empty-after-trim entry, a control character, and a duplicate (indexed
   `{label}.permissions[{j}]` messages); 3 new tests +
   `toml_round_trips_with_defaults_for_every_omitted_key` extended with a
   `permissions = [...]` stage.
4. **`agent_task.rs`** — `template_vars` over the same private `parse` that
   rendering and validation use (order, deduped); the literal-brace defect
   pinned by `a_value_full_of_braces_is_never_reparsed` (GraphQL-shaped body,
   incl. through the built-in Issue prompt),
   `braces_in_a_value_cannot_inject_a_placeholder`, and
   `template_vars_lists_what_a_stage_prompt_needs` (empty → `[]`, dedup, `{{`
   literal, unterminated → `Err`).
5. **`store/notification.rs` + `db_notification.rs`** —
   `stamp_dispatch_run(id, session_id, artifact_path)` beside
   `update_dispatch_status`; SQL `UPDATE agent_dispatches SET session_id=?1,
artifact_path=?2 WHERE id=?3`; no schema change, `SCHEMA_VERSION` untouched.
6. **`db_tests.rs`** — `stamp_dispatch_run_records_the_session_and_artifact`:
   pipeline row with a parent, stamp, read back (session + artifact set;
   status/stage/parent untouched); stale id is a no-op `Ok(())` with the why.
7. **`config/config.toml.example`** — `permissions` in the
   `[[pipeline.stages]]` key table + a `permissions = [...]` line in the
   commented `code` stage (this is also what satisfies the config_example
   drift test, which registers keys from `key = …` lines).
8. **`openspec/changes/improve-agent-pipeline-v2/`** — proposal (cites group Q
   - THE-76), design (D1–D6 ported + invariants), tasks (3 serial chunks,
     chunk-1 items checked), delta specs `specs/agent/spec.md` (stage dispatch,
     permission seeding, run-completion contract, wake primitive, registry
     freshness) and `specs/cli/spec.md` (open --stage, gated done, verify, wait,
     session close/list --live). All six pilot-failure scenarios from the chunk
     are present as WHEN/THEN.

## Verification (scoped per the dev-loop policy)

- `just quick thegn-core` — clean (clippy `-D warnings`, lib+bin).
- `cargo nextest run -p thegn-core pipeline_run` — 21 passed.
- `cargo nextest run -p thegn-core config_pipeline` — 19 passed.
- `cargo nextest run -p thegn-core agent_task` — 41 passed.
- `cargo nextest run -p thegn-core dispatch` — 21 passed (db_tests roster).
- `cargo nextest run -p thegn-core --test config_example` — 2 passed (every
  key documented; example parses as Config).
- `--test env_overlay_coverage --test hm_module_drift` — 4 passed (the design
  doc's "unaffected but re-run" pair).
- `nix run .#openspec -- validate --all --strict` — 169/169 (the full suite;
  the change also validates individually). `just openspec-validate` itself
  needs the dev shell's PATH; run through the flake's pinned CLI instead.
- No new `let _ =` / `.ok()` anywhere in the diff (ratchet-safe by grep).
- Trait `stamp_dispatch_run` has exactly one implementor (`Db`) — verified by
  grep, so no other crate can break compiling against the new method.

## Unverified

Deferred to the review/pre-PR stage per the no-full-workspace-compile rule:

- `just lint` (full-workspace clippy + ratchets), `just test` (nextest across
  the workspace), `just coverage` (95% core gate) — not run; the scoped
  equivalents above are green. In particular the 95% line-coverage number for
  the new module is unmeasured (every public item has tests, but the gate is a
  `just coverage` run).
- Cross/feature/MSRV builds and `just smoke` — not run (chunk 2/3 touch the
  smoke script; the host crate was never compiled here).
- `tests/config_example.rs`'s ALLOWLIST was not touched — no new allowlist
  entry was needed since `permissions` is documented in the example — but the
  full `just ci` config-reference generation (keybindings/config docs) was not
  exercised.
- `SCHEMA_VERSION` unchanged asserted by inspection + the existing migration
  tests passing in the `dispatch` filter; not re-run as the whole
  `db_migrate` module (partially covered: `pre_v56_db_gains_the_dispatch_pipeline_columns_without_resetting_anything`
  ran green within the dispatch filter).
