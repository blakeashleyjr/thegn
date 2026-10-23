# CI logs as a resource, MCP projection, and autofix handoff

Linear: THE-48

## Why

The CI-inspection layer (AV Phase A) is complete as a _human_ surface: the
`CiProvider` seam (GitHub Actions via `gh`, GitLab CI via `glab`/API, the rest
`reserved`), the normalized run→job→step→log model in `thegn_core::ci`, the
panel `Section::Ci` + statusbar badge + cross-worktree `CiFailure` excerpts,
the badge-modal drill with a failing-log tail, the full `thegn ci`
runs/view/log/rerun/trigger/cancel/detect CLI, the `ci_runs_cache` table, and
the off-loop TTL'd/backoff refresh. THE-48's audit against the local-CI field
(act, gama, wrkflw) finds the gaps are not in inspection but in _projection_
and _action_:

1. **Logs are ephemeral.** Every log view is a fresh provider fetch
   (`CiDetailPayload` for the drill, a direct seam call for `thegn ci log`);
   nothing is cached, so a terminal run's immutable log is re-fetched on every
   look, and no other surface can serve it at all.
2. **CI has zero external projection.** No `ci.*` row exists in
   `thegn_core::capability::CATALOG`, so the control API, MCP, and plugins
   cannot read runs or logs. An agent asked to "fix CI" must shell out to `gh`
   itself — thegn already normalized this data and then kept it to the TUI.
   THE-48's headline is exactly this: "Expose all logs directly via thegn MCP."
3. **Autofix is narrower than it looks.** The agent-task engine already has
   `TaskKind::PrCiFailure`, but it only fires for _queued PRs_ (the PR queue),
   and its `{log}` variable is a list of check **URLs** ("the forge's rollup
   carries no log text") — the agent is pointed at a browser page it cannot
   read. A red run on the current branch with no queued PR has no handoff path
   at all.
4. **No log redaction exists anywhere.** The only redaction in the tree is the
   key-name config scrub in `mcp::docs::redact`. CI log _content_ routinely
   carries token-shaped strings, and today's prompt/URL paths sidestep the
   question only because they never carry log text. Exposing logs to agents
   and MCP clients without a scrubber would be a secret-exfiltration surface.

On local execution (act/gama/wrkflw): gama is an API-driven runs TUI — thegn's
existing inspection layer already covers that ground. act and wrkflw _execute_
workflows locally inside container runtimes with their own event synthesis,
secrets plumbing, and image management. That is a workflow-engine product, not
a worktree-IDE feature; embedding one would violate "seams, not vendors" in
the largest possible way. thegn's lane is launching and surfacing — the same
judgment as the `[[agents]]`/`[[tools]]` picker.

## What Changes

1. **CI job logs become a first-class cached resource.** New `ci_log_cache`
   table (worktree, run_id, job_id → text, truncated, fetched_at). When the
   off-loop refresh sees a run reach a terminal **failed** state, it fetches
   and caches the failing jobs' log tails (bounded by `[ci] log_tail_lines`
   and a new `[ci] log_cache_runs` retention knob). The drill, `thegn ci log`,
   and every new surface below serve from the cache first; a terminal run's
   cached log is never re-fetched.
2. **Logs are redacted at ingest.** New pure `thegn_core::ci_redact` scrubber
   (token shapes: `ghp_`/`github_pat_`/`glpat-`, AWS key ids, JWTs, PEM
   blocks, `Authorization:` values, URL userinfo, `password/token/secret/
api_key = …` assignments) applied **before the cache write**, so secrets
   never rest in the state DB and every downstream consumer — TUI, CLI, MCP,
   control API, agent prompts — is safe by construction.
3. **CI joins the capability catalog.** Two read rows: `ci.runs` (cached run
   history per worktree, staleness carried) and `ci.log` (a cached job log +
   `first_failure_line`; defaults resolve to the latest failed run's first
   failing job so an agent needs zero ids). Projected on HTTP + CLI (the CLI
   verbs exist) + MCP; gRPC and plugin start as excused `SURFACE_GAPS`. MCP
   tools `ci_runs`/`ci_log` ride the parameterised state-tool infrastructure
   being built in `complete-control-surface-coverage` — a dependency, not
   re-scoped here.
4. **Autofix handoff for failed runs.** New agent-task kind `ci_failure`
   (vars: `branch`, `worktree`, `workflow`, `run_id`, `run_url`, `job`,
   `log` — a redacted excerpt centered on the first failure line). A new
   `[ci.autofix]` policy table: `mode = "off" | "suggest" | "auto"` (default
   `off`), `agent`, `attempts` (per head SHA), `prompt` override. `suggest`
   raises an actionable notification + a fix action in the Ci section/drill;
   `auto` dispatches without a keypress but only for runs whose head SHA is
   the worktree's current HEAD, never for a branch the PR queue or merge
   queue already owns.
5. **The PR queue's CI prompt gets real logs.** When the active `CiProvider`
   can resolve a failing check to a job, `TaskKind::PrCiFailure`'s `{log}`
   carries the redacted excerpt instead of bare URLs (URLs remain the
   fallback when no CI provider is configured).
6. **Local execution stays a configured tool.** A documented `[[tools]]`
   recipe for act/wrkflw in `config/config.toml.example` + `docs/help/`
   (launch in a pane, in the worktree, under the shared sandbox slice like
   every pane). Embedding a workflow executor is an explicit non-goal;
   roadmap AV 716 (streaming an `act` run into the run view) stays open and
   is not blocked by this change.

## Impact

- **Roadmap**: group **AV** (CI/CD inspection, items 698–717). This adds the
  MCP/autofix counterpart items to AV; AV 716 (local `act` runner) is
  deliberately _not_ absorbed — this change records the judgment that local
  execution is a tool entry today. Touches **AT 638/646** adjacent surfaces
  only via the PR-queue prompt upgrade.
- **Specs**: `ci-inspection` (log resource, redaction, autofix, local-runner
  boundary), `capability-catalog` (two `ci.*` rows), `state-db`
  (`ci_log_cache`).
- **DB schema change: `user_version` bump** for `ci_log_cache`.
- **In-flight changes reconciled**:
  - `complete-control-surface-coverage` (THE-39) — **dependency** for
    parameterised MCP tools and the SURFACE_GAPS ratchet; the two new rows add
    excused gRPC/plugin cells, which that change's ratchet-regeneration flow
    must absorb (coordinate, don't fork the policy).
  - `add-agent-task-engine` (implemented) — the `ci_failure` kind is a plain
    engine extension; pinned kind-count tests must be updated.
  - `add-pr-queue` (implemented) — `PrCiFailure` prompt upgrade only; queue
    semantics untouched.
  - `add-issue-autopilot`, `add-watched-pr-comment-tasks` — consumers of the
    same engine; no overlap in scope.
  - `add-mcp-proxy-hub` — the hub would aggregate `thegn mcp serve` like any
    upstream; nothing here assumes it.
- **New action ids** (fix-with-agent in the Ci section/drill) and a new
  notification kind → `docs/help/` updates; the help + help-prose ratchets
  gate them. New config keys documented in `config/config.toml.example`.
- The shell stays AI-free: autofix is the generic subprocess hook (engine +
  `[[agents]]`), default off; every read surface works with no agent
  configured.

## Non-goals

- Embedding a local workflow executor (act/wrkflw) or a `CiSystem::Local`
  provider.
- Live streaming of in-flight job logs (providers expose complete-job logs;
  the drill's bounded poll while open — AV 710 — is the honest ceiling).
- New CI providers (Drone/Woodpecker/Jenkins/Argo stay `reserved`).
- LLM-based failure explanation — "why did it fail" remains the deterministic
  scan.
- Write-side CI verbs (`ci.rerun`/`ci.trigger`) on the external surfaces —
  rerun/trigger/cancel stay TUI/CLI; a later change can add them behind
  `Scope::Write` if demanded.
