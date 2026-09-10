# Design — improve-agent-pipeline-v2

Ported from the THE-76 architect design (§2). The one-sentence problem: the
Lead does by hand, per worker, what thegn already has all the parts for. Each
decision below picks where the mechanism lives so the Lead keeps only the
judgment.

## D1 — Stage dispatch is composed **in the CLI process**, not in the daemon

"Server-side" in the issue means _thegn-side rather than Lead-side_. It does
not have to mean _inside the pane daemon_, and it should not:

- the CLI process already has the layered config (`main.rs` `--set`/`--config`
  layering) — the daemon has a possibly-stale boot snapshot, which is the very
  defect item 7 is about;
- the CLI process already opens the roster DB directly for every `dispatch`
  verb; adding a roster write inside the daemon would need new control-wire
  types, a new route, a `docs/api/control-v1.json` snapshot bump and three more
  surface projections, for zero capability;
- the only step that genuinely needs the daemon — spawning the session — is
  already a client call (`client.open`).

So `session open --stage` is a composition in `cmd/session.rs` over
`sessions.open` + the local roster + the local filesystem. No new control
capability, no wire change.

## D2 — Only `done` is gated, and only for rows that carry an artifact

`dispatch set-status <id> done` is gated on the row's `artifact_path` existing
and being tracked by git. A row whose `artifact_path` is `NULL` is **not**
gated: plain (non-pipeline) dispatches predate stages entirely, and gating them
would break `set-status done` for every non-pipeline user with no failure it
could possibly catch. `failed`, `abandoned`, `merged` are never gated — a
supervisor must always be able to record a bad outcome.

Uncommitted changes in the worktree are **reported, never blocking**. The
tracked check already catches the pilot's real failure (the worker wrote a file
and never committed it), and a dirty tree is legitimate mid-review. Report it
and let the Lead judge.

## D3 — `dispatch verify` / `dispatch wait` are CLI-only catalog rows

Both read local git + the local roster (`verify`) or compose the routed
`sessions.wait` (`wait`). Neither wants an HTTP route. The catalog's existing
shape for exactly this is `search.query`/`search.replace`: a row whose
`surfaces` is `SurfaceSet::of(&[Surface::Cli])`, declared in
`cli_control_caps()`. One catalog, no `SURFACE_GAPS` excuse, no route.

`session close` needs **no** new row: `sessions.kill` already exists and is
already covered by the CLI surface through `API_CALLS`. This item is pure
ergonomics — the `{"s": …}` positional-key trap from the pilot notes is exactly
the thing a named verb removes.

## D4 — Item 7 is solved by a **per-request registry refresh**, not a reload verb

The issue offers either. Take the per-request read, at the one place that is
actually stale — agent resolution in `daemon/agent_open::resolve`, which
already runs on `spawn_blocking`. Reasons:

- a reload verb only helps a Lead that remembers to call it; a per-request read
  removes the failure mode instead of documenting a workaround;
- a reload verb needs `DaemonService.config: Arc<Config>` to become swappable
  across every read site, plus a `Verb`, a catalog row, a route, a report type
  and a `control-v1.json` snapshot bump — a lot of surface for a problem the
  read closes outright.

**Only the registry is refreshed** — `agents`, `tools`, `pipeline` — merged
over the daemon's boot config (`pipeline_run::with_fresh_registry`). That is
deliberate: the daemon may have been started with `--set`/`--config` overrides,
and a wholesale re-load would silently discard them. A stage's `agent` name is
the only thing that goes stale in practice, so refresh exactly that and nothing
else.

## D5 — Row-before-session ordering

`session open --stage` inserts the roster row _before_ it opens the session. If
the process dies between the two, the operator is left with a visible `queued`
row (re-drivable) rather than a live agent nobody has a record of. The row is
stamped with `session_id` + `artifact_path` and moved to `running` after the
open returns; if the open _fails_, the row is moved to `failed` and its id is
named in the error.

This needs one new roster write, `stamp_dispatch_run(id, session_id,
artifact_path)` — the roster had no field update at all until now. It stays a
_store_ method, not a capability: it is an internal step of a CLI verb, not an
external door. (Exposing it externally would let a remote caller rewrite a
row's artifact pointer — a durability hazard for zero demonstrated need.)

## D6 — Artifact paths are sanitized, per-issue, row-keyed

`.thegn/pipeline/<ISSUE>/<stage>/<row>.md`, where `<ISSUE>` is the roster
issue id with its `<provider>:` prefix stripped (`linear:THE-76` → `THE-76`).
Both `<ISSUE>` and `<stage>` are whitelist-sanitized to `[A-Za-z0-9._-]`,
because this string is joined under a worktree path and written to. `..`, `/`,
`\` and control characters must not survive — a tracker key is attacker-adjacent
data (an issue title is not, but an id from a misconfigured provider is cheap to
defend). The row id makes parallel coders of one stage collide-free.

## Invariants

- **0% idle** — every new verb is a one-shot CLI process; nothing touches
  `run.rs`, the render path, or the loop.
- **thegn-core stays substrate-free** — `pipeline_run` is pure: no tokio, no
  subprocess, no filesystem; facts are gathered by the host and passed in.
- **One capability catalog** — the new rows are CLI-only, declared in
  `cli_control_caps()`; no `SURFACE_GAPS` entries.
- **git is the source of truth** — the roster stores a pointer; the verify gate
  asks git whether the artifact is real.
- **Ignored `Result`s** — the permissions write and the roster stamps sit on
  the primary path of a user-invoked action; they surface, never `let _ =`.
- **Render decision** — untouched (no `run.rs` change; no new wake sources).
