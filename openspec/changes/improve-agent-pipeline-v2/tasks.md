# Tasks — improve-agent-pipeline-v2

Three chunks, **strictly serial**: chunks 2 and 3 both edit
`thegn-host/src/cmd/session.rs` and `test/smoke.sh`, and both depend on chunk
1's core API.

## 1. Core policy, config, roster write (thegn-core, config/, openspec/)

- [x] 1.1 `pipeline_run.rs`: `issue_key` / `artifact_path` (whitelist
      sanitization, traversal-proof, idempotent) + the run-completion verdict
      `verify_report` + wait-target selection `wait_candidates` (waitable =
      `Spawning | Running`, each miss its own error) + the pure
      `merge_claude_allow` (refuses malformed/wrongly-shaped files, unions
      allow lists, preserves unrelated keys) + `with_fresh_registry`. Every
      public item unit-tested; nothing impure.
- [x] 1.2 `[[pipeline.stages] permissions` field + `validate_pipeline`
      checks (empty / control char / duplicate) + documented in
      `config/config.toml.example` (config_example drift test green).
- [x] 1.3 `agent_task::template_vars` (same parser as render/validate) + the
      literal-brace regression tests (a value is never re-parsed; a
      placeholder-looking value cannot inject).
- [x] 1.4 `stamp_dispatch_run` on the store trait + `Db` impl + DB test
      (fields stamped, status/stage/parent untouched, stale id a no-op `Ok`);
      `SCHEMA_VERSION` unchanged.
- [x] 1.5 OpenSpec change folder (this one), strict-validated.

## 2. Run-completion contract (thegn-host, depends on 1)

- [ ] 2.1 `dispatch verify` — prints the `VerifyReport` (ok / reasons / dirty)
      for one row; CLI-only catalog row `dispatches.verify` (`Verb`,
      `required_scope`, `cli_control_caps()`; no route, no `SURFACE_GAPS`).
- [ ] 2.2 Gated `dispatch set-status done` — rows carrying an artifact are
      refused unless it exists and is tracked; reasons printed verbatim; dirty
      reported; `failed`/`abandoned`/`merged` and no-artifact rows ungated.
- [ ] 2.3 `dispatch wait` — waits on one row's session or `--any`; catalog row
      `dispatches.wait`; composes the routed `sessions.wait` (already answers
      instantly from a tombstone).
- [ ] 2.4 `test/smoke.sh` cases for the gate (untracked artifact refused, no
      artifact passes) + help-page update for the new verbs (help ratchet).

## 3. Stage dispatch + liveness (thegn-host, depends on 1 and 2)

- [ ] 3.1 `session open --stage <stage>` — renders the stage prompt with
      `template_vars`-checked bindings (empty render ⇒ refuse, no session),
      sanitizes the artifact path (`pipeline_run::artifact_path`), seeds
      permissions, inserts the row, opens the session, stamps
      `stamp_dispatch_run`, moves to `running`; open failure ⇒ row `failed`
      with its id named.
- [ ] 3.2 `session close` (named verb over `sessions.kill`) and
      `session list --live` (exited sessions carry `exited_at_ms` /
      `exit_code` / `final_state`).
- [ ] 3.3 Daemon registry freshness — agent resolution refreshes `agents` /
      `tools` / `pipeline` per request via `with_fresh_registry` (D4), merged
      over the boot config.
- [ ] 3.4 `test/smoke.sh` cases + help-page update; re-record e2e snapshots if
      any frame changed (none expected).
