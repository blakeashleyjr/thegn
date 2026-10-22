# Tasks — CI logs as a resource, MCP projection, autofix handoff

## Phase 1 — core (pure; carries the 95% line gate)

- [x] 1. `thegn_core::ci_log::redact` — pattern scrubber
     (`ghp_`/`github_pat_`/
     `glpat-`, AWS key ids, JWTs, PEM fences, `Authorization:` values,
     URL userinfo, credential-named `k = v`); unit tests per pattern class +
     false-positive guards (benign lines untouched); as built in
     `crates/thegn-core/src/ci_log.rs`
- [x] 2. Excerpt windowing — bounded window around
     `CiLog::first_failure_line` for prompts and payloads; unit-tested
     (marker at start/end/absent); as built: `ci_log::failure_excerpt` for
     prompts (`failure_excerpt_marker_at_start_end_and_absent`) and
     `ci_log::bounded_window` for stored/served logs
     (`bounded_window_keeps_a_failure_older_than_the_tail`), called from
     `ci_autofix`, `CiLogEntry::new`, drill, `thegn ci log`, and `ci.logs`
- [x] 3. `[ci] log_cache_runs` config key (default 10, 0 = off) +
     `[ci.autofix]` table (`mode` off/suggest/auto, `agent`, `attempts`,
     `prompt`) — `config_ci.rs`, validation (unknown agent name, mode enum),
     config tests
- [x] 4. `TaskKind::PrCiFailure` (`"pr_ci_failure"`) in `agent_task` —
     prompt variables (as built in `crates/thegn-core/src/agent_task.rs`):
     `branch`/`worktree`/`workflow`/`run_id`/`run_url`/`job`/`log`, default
     prompt, template validation, and pinned kind-count tests
- [x] 5. Autofix policy decision fn — (mode, head SHA vs HEAD, attempt
     budget, PR-queue/merge-queue ownership) → Skip/Suggest/Dispatch;
     exhaustive unit tests
- [x] 6. `Verb::CiRuns` / `Verb::CiLogs` + catalog rows (`ci.runs`,
     `ci.logs`; as built in `crates/thegn-core/src/capability.rs`).
     `Scope::Read`; HTTP/CLI/MCP surfaces, with gRPC/plugin in `SURFACE_GAPS`;
     every-verb pins and `required_scope` tests
- [x] 7. `db`: `ci_log_cache` table + `user_version` bump + retention-evict
     store fns; migration test; writer funnel test (only the redaction
     chokepoint path writes rows)

## Phase 2 — svc/host: cache population and read surfaces

- [x] 8. Refresh transition hook — off-loop terminal-failure detection in the
     CI refresh populates `ci_log_cache` (first N failing jobs) via
     `CiProvider::logs`; no new wake source; backoff/health notes reused
- [x] 9. Cache-first reads — drill `CiDetailPayload` and `thegn ci log` serve
     terminal runs from cache, live-fetch only on miss; `truncated`/redacted
     indicators rendered
- [x] 10. Daemon handlers — `ci.runs` (parse `ci_runs_cache`, skip bad rows,
      carry `fetched_at`) and `ci.logs` (cache read; as built, a cache miss
      falls back to an off-loop provider fetch, with no not-cached error;
      argument defaulting to latest failed run / first failing job), modeled
      on `pr_status`; control HTTP route + client
- [x] 11. MCP `ci_runs`/`ci_logs` state tools on the parameterised-tool
      infrastructure from `complete-control-surface-coverage` (**dependency —
      land after it**; as built in `crates/thegn-core/src/mcp/state.rs`);
      tool schemas + `mcp_tools_cover_catalog` pins;
      regenerate SURFACE_GAPS through that change's sanctioned path
- [x] 12. `thegn ci runs --json` / `ci logs` claim the CLI cells in the
      per-surface coverage tests (as built in `crates/thegn-host/src/cmd/ci.rs`)

## Phase 3 — autofix handoff + PR-queue prompt upgrade

- [x] 13. Autofix driver — refresh hook evaluates the policy fn; `suggest`
      raises the new notification kind + fix action (Ci section/drill);
      `auto` dispatches via `agent_run` (worktree, watchdog, shared slice);
      attempt budget persisted per head SHA
- [x] 14. `PrCiFailure` `{log}` upgrade in
      `pr_driver::compose_with_log` — redacted cached excerpt (as built in
      `crates/thegn-host/src/pr_driver.rs`); `check_urls` fallback; update the
      pinned prompt tests
- [x] 15. New action id(s) + notification kind → keymap/palette wiring and
      `ACTION_SPECS`

## Phase 4 — docs, help, config example

- [x] 16. `config/config.toml.example`: `log_cache_runs`, `[ci.autofix]`
      (with the prompt-injection caution next to `mode`), and the act/wrkflw
      `[[tools]]` recipe
- [x] 17. `docs/help/` — claim the new action ids + notification kind and
      document the CI log cache, MCP tools, and autofix policy;
      `just help-ratchet-update` only if pinned debt genuinely shrinks
      (never grow the allowlists)
- [x] 18. `docs/help/cli.md` documents `ci runs` / `ci logs` and cache
      behavior (as built)
- [x] 19. Smoke: `thegn ci log` cache-first behaviour; e2e only if a frame
      changes (re-record with `just e2e-update`) (as built: `test/smoke.sh`
      seeds one `ci_log_cache` row and checks `thegn ci log` serves it from the
      cache with no provider, re-redacts it, and never labels a miss as cached;
      no frame changed, so no e2e re-record)

Remaining: task 20 (the pre-PR gate).

## Final

- [ ] 20. Run `just ci` once (includes openspec validate) as the pre-PR gate
