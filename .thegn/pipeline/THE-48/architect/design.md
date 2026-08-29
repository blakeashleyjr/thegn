# THE-48 — CI integration audit: logs via MCP + autofix

## Outcome

Add a bounded, redacted CI-log cache behind the existing `CiProvider` seam; make
the cache the read path for the Work-tab drill, `thegn ci logs`, control API, and
MCP; and add an opt-in CI-failure handoff that reuses the existing PR CI task
kind and agent runner. The provider remains the authority for run/job state and
log acquisition. SQLite is a best-effort read-through cache, never a source of
truth and never a reason to block the compositor.

The change is deliberately three serial, file-disjoint chunks:

```text
forge provider (off-loop, bounded subprocess)
        │
        ▼
core tail + redaction ──► SQLite ci_log_cache ──► control/CLI/MCP + Work drill
        │                                         │
        └──── new failed (run, job, head SHA) ────┴──► suggest/auto handoff
                                                        │
                                                        ▼
                                              existing PrCiFailure prompt engine
                                              + existing agent_run process seam
```

No migration or built binary may be run against the live state DB. Any manual
`thegn` invocation from this worktree must set `XDG_STATE_HOME` to a newly made
temporary directory.

## Verified current state

The OpenSpec change `openspec/changes/add-ci-logs-and-autofix/` was treated as a
draft and rechecked against this branch.

Already satisfied:

- The provider-neutral run/job/step/log model and deterministic failure scanner
  are in `crates/thegn-core/src/ci.rs:1-8,81-118,264-317`.
- `CiProvider` is an object-safe blocking seam, with GitHub and GitLab
  implementations and reserved Drone/Woodpecker/Jenkins/Argo kinds in
  `crates/thegn-svc/src/ci.rs:23-98,177-233,381-454`. Vendor commands stay in
  that implementation module.
- The Work-tab CI list, run drill, statusbar badge, rerun/cancel actions, and
  off-loop refresh/backoff/waker path already exist in
  `crates/thegn-host/src/panel/sections/ci.rs:65-233`,
  `crates/thegn-host/src/detail/ci_drill.rs:1-22`,
  `crates/thegn-host/src/actions.rs:224-350`, and
  `crates/thegn-host/src/ci_refresh.rs:101-243`.
- Run-history caching already exists as `ci_runs_cache` in
  `crates/thegn-core/src/db.rs:473-487`, `db_cache.rs:84-104`, and the CLI
  already has `ci runs/view/log/rerun/trigger/cancel/detect` in
  `crates/thegn-host/src/cmd/ci.rs:23-107`.
- The generic prompt/command engine and process runner already exist in
  `crates/thegn-core/src/agent_task.rs:26-113,401-472` and
  `crates/thegn-host/src/agent_run.rs:97-220`. `PrCiFailure` already carries
  PR context and a `{log}` variable; `pr_driver.rs:553-634` is the established
  handoff path.
- Generic configured tools and local reproduction documentation already exist;
  `config/config.toml.example:1112-1147` and `docs/local-ci.md` are the right
  boundary for `act`, `gama`, and `wrkflw`.

Missing or only partially satisfied:

- `actions.rs:284-350` and `cmd/ci.rs:255-277` fetch live log text and do not
  cache or redact it. `ci_refresh.rs:184-243` refreshes only run history.
- The current database is schema v61 (`db.rs:131-136`) and has no per-job log
  table. `CacheStore` only exposes run-history cache methods
  (`store/cache.rs:14-28`).
- There are no `ci.runs`/`ci.logs` catalog rows, control verbs/routes/client
  methods, or MCP state tools. The current catalog has `pr.status` at
  `capability.rs:318-323`; the current control verb list has `PrStatus` at
  `control.rs:286-287`; MCP state capabilities are listed at
  `mcp/state.rs:347-365`.
- The PR queue currently fills `PrCiFailure`’s log with check URLs rather than
  log content (`pr_driver.rs:603-614`).
- Existing `log_redact.rs:1-14` protects command/argv diagnostics; it is not a
  CI-log-content redaction path.

## Decisions and invariants

### Provider seam and bounded acquisition

Keep `CiProvider::logs(&GitLoc, run_id, job_id) -> CiLog` unchanged. The only
forge-specific command changes belong in `crates/thegn-svc/src/ci.rs`:

- GitHub continues to use the `gh run view --job ... --log` family inside
  `GithubCi::logs`; GitLab continues to use its trace API inside `GitlabCi::logs`.
  No `gh`, `glab`, HTTP endpoint, or local-runner name leaks into core, control,
  MCP, or the panel.
- Replace the unbounded log command collection used by the log implementations
  with a bounded tail collector. It must retain at most the configured line tail
  and a hard byte ceiling while the child is being drained, mark truncation, and
  kill/finish within the existing blocking worker. A successful provider call
  still returns a normal `CiLog`; provider failure degrades to the cache or an
  inline error.
- Keep unsupported providers reserved. Do not add `act`, `gama`, or `wrkflw` to
  `CiProvider`, its factory, or the control catalog. The audit result is that a
  local runner is a command recipe, not a cheap provider: it has different event,
  credential, job identity, and log semantics. Document optional recipes in
  `docs/local-ci.md` through the existing `[[tools]]` mechanism, with each vendor
  binary named only in its recipe/documentation. No new vendor dependency.

### Core log contract

Create a sibling `thegn-core/src/ci_log.rs` module rather than growing `ci.rs`.
It owns pure functions and serializable cache-facing types:

- `CiLogEntry { worktree, run_id, job_id, job_name, text, truncated,
redacted, fetched_at }` (or equivalent wire-safe shape). `text` is always the
  bounded tail; no public path may expose raw provider text.
- A deterministic tail/byte-bound function that preserves complete UTF-8 lines,
  keeps the newest content, reports whether bytes or lines were removed, and
  never treats `0` as “unlimited”.
- A deterministic redactor applied before SQLite, prompt composition, terminal
  display, CLI output, control output, and MCP output. Cover bearer/auth headers,
  `Authorization` values, URL userinfo, JWT-like values, AWS key IDs, PEM blocks,
  and common `KEY=value`/token/password assignments. Preserve line structure and
  replace values with the canonical redaction marker. Tests must prove secrets
  do not survive through each supported shape and that ordinary diagnostics do.
- A pure stable key/dedupe predicate for `(worktree, run_id, job_id, head_sha)` and
  a pure retention selector. These let the host decide whether a terminal failed
  job is new without putting policy in SQLite or the event loop.

Add `[ci] log_cache_runs` (default 10; `0` disables log persistence/fetch) and a
hard-bounded `log_max_bytes` (default documented value) alongside the existing
`log_tail_lines`. Add `[ci.autofix] mode = "off" | "suggest" | "auto"`, default
`off`. The global autofix policy is only a default: the trusted
`[workspace.<slug>]` configuration gets a small `CiAutofixOverlay` with the mode
override, and the resolver exposes `Config::repo_ci(repo_root)`. A checked-in
repo `.thegn.*` file must not be able to enable an agent or widen this policy.
Agent selection, prompt, isolation, timeout, and attempt ceiling reuse the
effective per-repo `PrQueueConfig` (`repo_pr_queue`), because that is already the
trusted per-repo configuration and the existing `PrCiFailure` contract. Do not
add duplicate `[ci.autofix].agent`, prompt, or attempts keys.

The new keys are config-schema keys: put them in `config/config.toml.example`
and `docs/help/configuration.md`. Because they are policy/cache settings rather
than safe runtime knobs, pin them with explicit reasons in
`test/env-overlay-ratchet.txt`; do not silently omit them from the ratchet.

### SQLite cache and migration

Add a v62 additive migration for a table equivalent to:

```sql
CREATE TABLE IF NOT EXISTS ci_log_cache (
  worktree   TEXT NOT NULL,
  run_id     TEXT NOT NULL,
  job_id     TEXT NOT NULL,
  job_name   TEXT NOT NULL,
  head_sha   TEXT NOT NULL DEFAULT '',
  text       TEXT NOT NULL,
  truncated  INTEGER NOT NULL DEFAULT 0,
  redacted   INTEGER NOT NULL DEFAULT 1,
  fetched_at INTEGER NOT NULL,
  PRIMARY KEY (worktree, run_id, job_id)
);
CREATE INDEX IF NOT EXISTS idx_ci_log_cache_worktree
  ON ci_log_cache(worktree, fetched_at);
```

The exact DDL may use the repo’s established naming/style, but it must be
additive, idempotent, and verified before `user_version` is stamped. Bump the
schema ladder in `db.rs`, add the v62 verifier and pre-v62 migration test in
`db_migrate.rs`, and keep query/retention bodies in a new `db_ci.rs` sibling.
Expose cache reads/writes/deletes through the object-safe store seam. On a
worktree deletion, delete both run and log cache rows. Cache writes are
best-effort and must never turn a valid provider result into a user-visible
failure.

Retention runs after a successful run-history refresh: retain logs for the most
recent configured terminal runs for that worktree, delete older rows, and never
delete another worktree’s rows. If caching is disabled, serve a bounded live
tail for an explicit request but do not persist it.

### Off-loop refresh and read paths

Extend `ci_refresh` so the existing `spawn_bg`/blocking path does this sequence:

1. Read the old cached run list.
2. Fetch the bounded run list through `CiProvider::runs` and atomically-ish
   replace `ci_runs_cache` as today.
3. For newly observed terminal failed jobs, fetch logs in bounded batches, run
   core tail/redaction, and upsert the cache. A failed log fetch leaves the run
   status and any older cache intact.
4. Apply retention, record health/backoff, and pulse `TerminalWaker` exactly as
   the current refresh does.

The Work-tab list remains instant from `ci_runs_cache`. Enter/detail first reads
`ci_log_cache`, then schedules a bounded provider fetch only for missing/stale
jobs; the overlay must display cached content while the refresh is pending. The
same cache-first policy applies to `thegn ci logs`, control API `ci.logs`, and
MCP `ci_logs`. A control/MCP cache miss may perform the bounded provider call in
the daemon’s blocking lane and persist the result; no terminal or MCP request
may call a vendor binary on the compositor thread.

### Capability catalog and control/MCP projection

Add exactly two read rows to the one catalog:

- `ci.runs` — cached run list/run metadata, parameterized by worktree/limit.
- `ci.logs` — bounded redacted job-log entries, parameterized by worktree,
  run, job, and optional tail selection.

Use `SurfaceSet::of(&[Surface::Http, Surface::Cli, Surface::Mcp])`: those are the
promised surfaces for this issue. Do not add gRPC/plugin `SURFACE_GAPS` lines to
excuse an accidental `ALL` declaration; deliberate non-exposure belongs in the
surface set. Add `CiRuns` and `CiLogs` to `Verb::ALL`, map both to `Scope::Read`,
and add HTTP routes/client calls/daemon projections plus MCP state-tool specs.
The existing parameterized `StateToolSpec`/`ArgSpec` infrastructure in
`thegn-core/src/mcp/state.rs:24-52,76-96` is sufficient; the OpenSpec’s claimed
dependency on a future parameterized-tool change is stale on this branch.

Control wire types must contain only bounded/redacted cache data and explicit
`fetched_at`/`truncated`/`redacted` metadata. Update
`docs/api/control-v1.json` with the repository’s snapshot mechanism in the same
chunk as the control types. Keep gRPC proto files unchanged and keep plugins
outside this capability’s declared surface.

Project the existing CLI command as `thegn ci logs` (retain `ci log` as a
compatibility alias), cache-first, with valid JSON on `--json`; update completion
catalog entries and `test/completion-slot-ratchet.txt` for both the new plural
spelling and its required IDs. No new provider-specific CLI command is allowed.

### Autofix handoff

Create a host sibling `ci_autofix.rs`. It consumes a redacted cache entry and
the effective PR cache/queue context, then calls the existing agent process seam
with `TaskKind::PrCiFailure`, `TaskVars`, the effective `PrQueuePrompts`, and
`agent_run::run`. It must not create a new AI dependency or a second prompt
renderer.

Rules:

- A candidate is `(repo/worktree, run_id, job_id, head_sha)`. Require a known PR
  cache/queue item, a non-empty head SHA, and an unchanged local/remote PR head
  before `auto`; missing context degrades to a visible suggestion/unavailable
  note, never an unsafe dispatch.
- `off` never hands off. `suggest` records one deduped notification/action for
  the human. `auto` dispatches only when the current head matches and the
  effective PR queue has an agent configured. Reuse the PR queue’s ownership,
  isolation, timeout, and `agent_max_attempts` policy; do not race its driver or
  dispatch the same `(PR, SHA)` twice.
- Persist the dedupe/attempt marker in the v62 cache-side table or an equivalent
  additive cache row, keyed by worktree/run/job/head SHA. Mark the handoff before
  spawning so refresh races cannot double-spend; clear/refill only on a new head
  according to the existing PR queue reset rules. A provider retry/rerun is not
  an agent dispatch.
- Use an existing failure notification kind and source reference, not a new
  notification enum/priority ratchet. The Work CI detail and row action provide
  the explicit “fix” action; all mutation work is off-loop and waker-pulsed.
- `pr_driver`’s `{log}` value becomes the bounded redacted excerpt(s), with the
  existing check-URL fallback when no log cache exists. This preserves existing
  PR queue behavior while making the new evidence useful to its same prompt.

### UI and help

Keep the current Work-tab list/detail layout and caps-aware rendering. Add only
the metadata/action needed to show “cached”, “tail/truncated”, “redacted”, and
“fix available/suggested”; route every new glyph through the existing glyph/caps
slots. Add `f` (or another unclaimed, documented key) only if the action table,
detail action enum, run dispatch, `panel/section_keys.rs`, and help ratchet are
updated together. Prefer the existing `v`/Enter drill and an explicit `f` fix
action over opening a pane or browser.

Update `docs/help/panel.md` for CI log list/detail and fix semantics,
`docs/help/cli.md` for `ci logs`, `docs/help/configuration.md` for the new
settings, and `docs/local-ci.md` for the optional configured-tool recipes and
the fact that local tools are not CI providers. Run the host help ratchet tests;
do not hand-edit generated help/config reference artifacts.

## Chunk dependency contract

The chunk files below are the implementation contract. They are deliberately
serial because later chunks consume types and routes from earlier chunks, but
their write sets do not overlap. A coder must not edit a file outside the
declared set without first returning for an architecture decision.

## Pruned OpenSpec claims

- Cut the proposed standalone `TaskKind::CiFailure`: `PrCiFailure` already has
  the required PR context, `{log}`, prompt validation, and process path. Adding
  another kind would duplicate policy and enlarge `ALL_KINDS` without a new
  capability.
- Cut gRPC/plugin gap excuses: the catalog surface set will intentionally omit
  those surfaces, consistent with `capability.rs:728-760` and the architecture
  rules.
- Cut embedded local CI providers and vendor engines: the existing provider
  seam is forge-oriented and the optional binaries belong in configured tool
  recipes only.
- Cut new AI/runtime dependencies, loop-side subprocesses, raw-log storage,
  unbounded provider collection, and repo-authored autofix enablement.
