# Design — CI logs as a resource, MCP projection, autofix handoff

## The log cache: redact at ingest, immutable once terminal

`ci_log_cache(worktree TEXT, run_id TEXT, job_id TEXT, text TEXT,
truncated INTEGER, fetched_at INTEGER, PRIMARY KEY(worktree, run_id, job_id))`,
`user_version` bump. Contract points:

- **Populated by the existing off-loop CI refresh** (`hydrate` /
  `ci_refresh`): when a run transitions to a terminal failed state (comparing
  the fresh listing against the cached one), fetch `CiProvider::logs` for its
  failing jobs — bounded to the first N failing jobs of the run — and write
  the rows. This runs on the same `spawn_blocking` + channel + `TerminalWaker`
  path as the run refresh; **no new wake source, no loop-side I/O**. Render
  impact: none beyond the existing refresh (`Full` when the panel repaints).
- **Cache-first everywhere.** The drill's `CiDetailPayload` and `thegn ci log`
  read the cache and only fall through to a live fetch on a miss (in-flight
  runs, non-failed jobs requested explicitly). A hit for a terminal run is
  final: logs of finished jobs are immutable upstream, so no TTL applies —
  retention, not freshness, bounds the table.
- **Retention**: keep logs for the most recent `[ci] log_cache_runs`
  (default 10, `0` disables caching) terminal-failed runs per worktree;
  eviction happens on write. Row text is already bounded by
  `[ci] log_tail_lines` (existing key). The DB stays a cache: dropping the
  table loses nothing git/the provider cannot restore.
- **Redaction happens before the cache write** (see Security). The cache
  stores only scrubbed text, so every consumer — including future ones — is
  safe by construction and secrets never rest in the state DB. The
  `truncated` flag and a `redacted` line-count travel with the text so the
  UI/MCP response can say what was withheld.

Rejected alternative: redact at egress (cache raw, scrub per surface). It
keeps the TUI byte-faithful but multiplies chokepoints (drill, CLI, HTTP,
MCP, prompt builder — and every future surface), and leaves secrets at rest
in `state.db`. The cost of ingest-time scrubbing is that the human's drill
view shows `***redacted***` where a token appeared; the raw line is one
`gh run view --log` away, which is the right friction.

## Catalog rows and the MCP projection

Two new `Verb`s / catalog rows, both `Scope::Read` (precedent:
`Verb::PrStatus`, which serves the PR cache):

| id        | payload                                                        |
| --------- | -------------------------------------------------------------- |
| `ci.runs` | cached normalized runs per worktree + `fetched_at` staleness   |
| `ci.log`  | one cached job log + `first_failure_line` + truncated/redacted |

- **Served from the daemon's DB handle** exactly like `pr_status`
  (`daemon/service.rs::with_db`): parse `ci_runs_cache` / read
  `ci_log_cache`, skip unparseable rows, carry `fetched_at`. The daemon never
  invokes a vendor CLI — a `ci.log` miss returns a distinct "not cached"
  error naming the CLI/TUI path that would populate it. (Fetch-on-miss in the
  daemon is an open question below, not Phase 1.)
- **Argument defaulting is the DX.** `ci.log` with only a worktree resolves
  to the latest failed run's first failing job; `ci.runs` defaults to the
  active worktree. An agent can go from "CI is red" to reading the failure
  with a single zero-id tool call.
- **Surfaces**: HTTP (new route), CLI (`thegn ci runs --json` / `thegn ci
log` already exist; the rows list Cli as implemented), MCP (`ci_runs`,
  `ci_log` via `CapId::tool_name`). gRPC ("not yet mirrored in
  control.proto") and plugin start as excused `SURFACE_GAPS` entries — the
  same excuse classes the table already carries. `required_scope` stays the
  single policy table; no per-surface policy is added.
- **Dependency**: parameterised MCP tools (per-tool JSON schemas) are being
  built in `complete-control-surface-coverage`. `ci_runs`/`ci_log` are
  specified against that infrastructure and land after it; if that change's
  SURFACE_GAPS ratchet file exists first, the new excused cells regenerate
  through its sanctioned update path.

## Autofix: policy before mechanism

The mechanism is entirely existing: `thegn_core::agent_task` renders the
prompt + resolves the command; `agent_run` executes it in the worktree under
the process-group watchdog and joins the shared `thegn.slice` via
`wrap_background_argv`. This change adds one `TaskKind::CiFailure`
(`"ci_failure"`; prompt vars `branch`, `worktree`, `workflow`, `run_id`,
`run_url`, `job`, `log`) and the policy that decides when it fires:

- `[ci.autofix] mode = "off"` (default): nothing happens. The feature is
  invisible until configured — the AI-free-shell rule.
- `"suggest"`: a failed run for a worktree's current branch raises a
  notification + a fix action (new action id, Ci section/drill). Dispatch
  requires the human keypress.
- `"auto"`: dispatch without a keypress, guarded by **all** of: the run's
  head SHA equals the worktree's current HEAD (never fix code the tree has
  moved past), an attempt budget per head SHA (`attempts`, default 1 — a new
  push refills it, mirroring the PR queue), and _ownership deconfliction_ —
  the branch is in neither the PR queue nor the merge queue (each of those
  already owns agent handoff for its entries; two drivers dispatching into
  one worktree is the failure mode).
- The `{log}` variable is a bounded excerpt (±~100 lines around
  `first_failure_line`) of the **already-redacted** cached text — the prompt
  never triggers a fresh fetch.
- The producer is the existing off-loop refresh: the same terminal-failure
  transition that populates the log cache evaluates the policy. No new
  thread, no new wake source.

`PrCiFailure` upgrade: `pr_driver::task_vars` consults the CI provider's
cached data for the PR's head SHA; when a failing check maps to a cached job
log, `{log}` becomes the redacted excerpt, else it stays `check_urls`. Pure
formatting change; queue semantics untouched.

## Local CI execution (act / gama / wrkflw): not our engine

- **gama** is a GitHub Actions TUI (list/trigger/watch over the API) — thegn's
  inspection layer is already the in-IDE equivalent and the AV group cites it
  as the inspiration.
- **act** runs workflows in Docker with event synthesis, image mapping, and
  secret files; **wrkflw** adds validation, DAG view, and docker/podman/
  emulation execution modes. Both are full workflow engines with their own
  container lifecycles.

Judgment: executing workflows locally is act/wrkflw's product, not a
worktree-IDE seam. A `CiProvider` impl wrapping act would be a vendor engine
behind a read-model trait it doesn't fit (no server, no run history, logs are
the process's stdout). The lane thegn owns is _launching and surfacing_:
ship a documented `[[tools]]` recipe (`wrkflw` / `act -j <job>` in a pane, in
the worktree, inside the shared sandbox slice like every pane). AV 716
(streaming an act run into the run view) remains a possible future change and
is neither claimed nor blocked here.

## Security

- **Threat: secrets in log content.** CI logs carry token-shaped strings
  (echoed env, `set -x` traces, URLs with userinfo, PEM blocks). Mitigation:
  `thegn_core::ci_redact::scrub` — pure, pattern-based, unit-tested in core —
  applied at ingest, before the cache write; downstream surfaces (TUI, CLI,
  HTTP, MCP, agent prompts) only ever see scrubbed text. Patterns are
  conservative-by-shape (provider token prefixes, AWS key ids, JWTs, PEM
  fences, `Authorization:` values, credential-named `k = v` assignments);
  false positives cost a masked line, false negatives are the provider's own
  masking backstop. The scrubber is a single chokepoint with a ratchet-style
  test pinning that no code path writes `ci_log_cache` except through it.
- **Threat: prompt injection via log text.** On public repos, log content is
  attacker-influenced (a fork PR's build output prints "ignore your
  instructions…"). The excerpt is data in a template, but no template can
  make an LLM treat it as data. Mitigations: `auto` mode's own-HEAD guard
  (fork-PR runs are not the worktree's HEAD), default `off`, attempt budget
  bounding blast radius, and the dispatched agent running in one worktree
  under the shared `thegn.slice` ceilings. `suggest` keeps a human between
  the log and the dispatch. This residual risk is documented in the config
  example next to `mode`.
- **Credentials**: unchanged — provider tokens stay `env:`/config
  (`[ci.gitlab] token`), never in the DB; the new surfaces add no credential
  storage. MCP/control exposure is read-only (`Scope::Read`) and serves only
  scrubbed cache content; no new write verb is projected externally.
- **New write surface**: only the agent dispatch, which is the existing
  engine's surface (worktree-scoped subprocess, watchdogged, resource-capped);
  this change adds a _policy gate in front of it_, not a new door. The
  external surfaces cannot trigger a dispatch.
- **Sandbox**: nothing new mounts or escapes; the local-runner recipe runs as
  an ordinary pane under existing pane sandboxing.

## Testability

- Core (95% gate): `ci_redact` patterns, retention/eviction policy fn,
  excerpt windowing around `first_failure_line`, `TaskKind::CiFailure` vars +
  default prompt render, autofix policy decision fn
  (mode × head-SHA × budget × ownership → Dispatch/Suggest/Skip), catalog
  row/scope pins, MCP tool-schema pins.
- Host/svc: daemon `ci.runs`/`ci.log` handlers (cache fixtures), refresh
  transition→cache-write, drill cache-first path; smoke covers `thegn ci log`
  cache behaviour.

## Open questions

1. Should the daemon ever fetch-on-miss for `ci.log` (spawning `gh`/`glab`
   from the daemon process)? Deferred: cache-only keeps vendor CLIs out of
   the daemon; revisit if agents hit misses in practice.
2. Should `suggest` mode be the default once the feature has soaked? (Ship
   `off`; flipping the default is a one-line follow-up with release notes.)
3. Do write verbs (`ci.rerun`) belong on MCP for an agent to verify its own
   fix? Deferred — the agent can push and let CI re-trigger, which is the
   safer loop.
