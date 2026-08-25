# Tasks — CI logs as a resource, MCP projection, autofix handoff

## Phase 1 — core (pure; carries the 95% line gate)

- [ ] 1. `thegn_core::ci_redact` — pattern scrubber (`ghp_`/`github_pat_`/
     `glpat-`, AWS key ids, JWTs, PEM fences, `Authorization:` values, URL
     userinfo, credential-named `k = v`); unit tests per pattern class +
     false-positive guards (benign lines untouched)
- [ ] 2. Excerpt windowing — bounded window around
     `CiLog::first_failure_line` for prompts and payloads; unit-tested
     (marker at start/end/absent)
- [ ] 3. `[ci] log_cache_runs` config key (default 10, 0 = off) +
     `[ci.autofix]` table (`mode` off/suggest/auto, `agent`, `attempts`,
     `prompt`) — `config_ci.rs`, validation (unknown agent name, mode enum),
     config tests
- [ ] 4. `TaskKind::CiFailure` (`"ci_failure"`) in `agent_task` — prompt vars
     `branch`/`worktree`/`workflow`/`run_id`/`run_url`/`job`/`log`, default
     prompt, template validation; update the pinned kind-count tests
- [ ] 5. Autofix policy decision fn — (mode, head SHA vs HEAD, attempt
     budget, PR-queue/merge-queue ownership) → Skip/Suggest/Dispatch;
     exhaustive unit tests
- [ ] 6. `Verb::CiRuns` / `Verb::CiLog` + catalog rows (`ci.runs`, `ci.log`,
     `Scope::Read`; surfaces HTTP/CLI/MCP, gRPC + plugin excused in
     `SURFACE_GAPS`); update every-verb pins and `required_scope` tests
- [ ] 7. `db`: `ci_log_cache` table + `user_version` bump + retention-evict
     store fns; migration test; writer funnel test (only the redaction
     chokepoint path writes rows)

## Phase 2 — svc/host: cache population and read surfaces

- [ ] 8. Refresh transition hook — off-loop terminal-failure detection in the
     CI refresh populates `ci_log_cache` (first N failing jobs) via
     `CiProvider::logs`; no new wake source; backoff/health notes reused
- [ ] 9. Cache-first reads — drill `CiDetailPayload` and `thegn ci log` serve
     terminal runs from cache, live-fetch only on miss; `truncated`/redacted
     indicators rendered
- [ ] 10. Daemon handlers — `ci.runs` (parse `ci_runs_cache`, skip bad rows,
      carry `fetched_at`) and `ci.log` (cache read + not-cached error;
      argument defaulting to latest failed run / first failing job), modeled
      on `pr_status`; control HTTP route + client
- [ ] 11. MCP `ci_runs`/`ci_log` state tools on the parameterised-tool
      infrastructure from `complete-control-surface-coverage` (**dependency —
      land after it**); tool schemas + `mcp_tools_cover_catalog` pins;
      regenerate SURFACE_GAPS through that change's sanctioned path
- [ ] 12. `thegn ci runs --json` / `ci log` claim the CLI cells in the
      per-surface coverage tests

## Phase 3 — autofix handoff + PR-queue prompt upgrade

- [ ] 13. Autofix driver — refresh hook evaluates the policy fn; `suggest`
      raises the new notification kind + fix action (Ci section/drill);
      `auto` dispatches via `agent_run` (worktree, watchdog, shared slice);
      attempt budget persisted per head SHA
- [ ] 14. `PrCiFailure` `{log}` upgrade in `pr_driver::task_vars` — redacted
      cached excerpt when resolvable, `check_urls` fallback; update the
      pinned prompt tests
- [ ] 15. New action id(s) + notification kind → keymap/palette wiring and
      `ACTION_SPECS`

## Phase 4 — docs, help, config example

- [ ] 16. `config/config.toml.example`: `log_cache_runs`, `[ci.autofix]`
      (with the prompt-injection caution next to `mode`), and the act/wrkflw
      `[[tools]]` recipe
- [ ] 17. `docs/help/` — claim the new action ids + notification kind and
      document the CI log cache, MCP tools, and autofix policy;
      `just help-ratchet-update` only if pinned debt genuinely shrinks
      (never grow the allowlists)
- [ ] 18. `docs/cli.md` / MCP serve doc text mention `ci_runs`/`ci_log`
- [ ] 19. Smoke: `thegn ci log` cache-first behaviour; e2e only if a frame
      changes (re-record with `just e2e-update`)

## Final

- [ ] 20. Run `just ci` once (includes openspec validate) as the pre-PR gate
