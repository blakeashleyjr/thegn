# THE-76 — Agent pipeline v2: the Lead's hand-rolled loop, made native

Architect design. Scope: the `[spec]`-tagged product items of THE-76 (items 1–7).
Out of scope: the board UI (THE-74), the ops/skill items.

## 0. The one-sentence problem

The Lead (a supervisor agent running `/pipeline`) does by hand, per worker, what
thegn already has all the parts for: render the stage prompt, seed permissions,
open a session, write a roster row, set it running — then poll a list, guess
whether the worker really finished, and mark it done. Every one of those steps
has a failure mode the pilot actually hit (see `pipeline-pilot-lessons`), and
each is a place where thegn owns the _mechanism_ and the Lead should keep only
the _judgment_.

This change moves the mechanism and nothing else. The doctrine established by
`add-agent-orchestration-surface` and restated at
`crates/thegn-core/src/config_pipeline.rs:1-22` holds unchanged: **thegn never
advances `next`, never enforces `concurrency`, never fires `timeout_secs`.**
What it gains here is the ability to (a) _perform_ one dispatch atomically when
asked, (b) _verify_ a claim about a finished row, and (c) _block_ until
something happens. Deciding what to dispatch, whether the verified result is
good, and what to do next stays the Lead's.

## 1. What exists today (evidence)

| Fact                                                                                                     | Evidence                                                                                                                                          |
| -------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Roster columns `stage`/`parent_id`/`session_id`/`artifact_path` exist (v56)                              | `crates/thegn-core/src/issue.rs:223-258`, `crates/thegn-core/src/db_notification.rs:299-321`                                                      |
| The roster has an **insert** and a **status update**, and nothing else                                   | `crates/thegn-core/src/store/notification.rs:152-171`                                                                                             |
| `dispatch set-status` validates the closed status set and the row id, and stops there                    | `crates/thegn-host/src/cmd/dispatch.rs:155-176`                                                                                                   |
| `[[pipeline.stages]]` carries `name/agent/prompt/concurrency/timeout_secs/next/on_blocked`               | `crates/thegn-core/src/config_pipeline.rs:44-73`                                                                                                  |
| Stage prompts are validated against `STAGE_VARS` but **never rendered by thegn**                         | `crates/thegn-core/src/agent_task.rs:129-150`                                                                                                     |
| The template engine substitutes values into the output and **never re-parses them**                      | `crates/thegn-core/src/agent_task.rs:302-321` (`render` walks `parse(template)`; `Piece::Var` pushes the value verbatim)                          |
| `session open` takes `--agent/--worktree/--prompt/--headless/--bind/--adopt` and returns a `SessionInfo` | `crates/thegn-host/src/cmd/session.rs:26-56, 251-295`                                                                                             |
| `--prompt` defaults to `""`, and empty ⇒ _interactive_ launch, silently                                  | `crates/thegn-host/src/cmd/session.rs:34-36`; `crates/thegn-host/src/daemon/agent_open.rs:57-58`                                                  |
| The daemon already returns exited sessions with `exited_at_ms` / `exit_code` / `final_state`             | `crates/thegn-svc/src/control/mod.rs:73-90`; `crates/thegn-host/src/daemon/service.rs:398-410`; `crates/thegn-host/src/daemon/tombstone.rs:78-98` |
| …but the CLI's listing line prints none of it                                                            | `crates/thegn-host/src/cmd/session.rs:144-162`                                                                                                    |
| `sessions.kill` is a routed capability with a client method — only the CLI verb is missing               | `crates/thegn-svc/src/control/routes.rs:63,160`; `crates/thegn-svc/src/control/client.rs:238`                                                     |
| `sessions.wait` on an already-dead session answers **immediately** from the tombstone                    | `crates/thegn-host/src/daemon/service.rs:764-784`                                                                                                 |
| Tombstones live `MAX_TOMBSTONES=32` / `TTL=10min`                                                        | `crates/thegn-host/src/daemon/tombstone.rs:37-45`                                                                                                 |
| The daemon holds a **boot snapshot** of config (`Arc<Config>`), and agent resolution reads it            | `crates/thegn-host/src/daemon/mod.rs:293`; `crates/thegn-host/src/daemon/service.rs:424`; `crates/thegn-host/src/daemon/agent_open.rs:38-64`      |
| CLI-only capabilities are a supported catalog shape                                                      | `crates/thegn-core/src/capability.rs:596-608` (`search.query`/`search.replace`, `SurfaceSet::of(&[Surface::Cli])`)                                |
| The CLI surface's implemented set is hand-listed beside the routed ones                                  | `crates/thegn-host/src/cmd/session.rs:505-561`                                                                                                    |
| `dispatch` verbs already open the DB directly, no daemon                                                 | `crates/thegn-host/src/cmd/dispatch.rs:85,126,166`                                                                                                |
| git facts are one helper away                                                                            | `crates/thegn-core/src/util.rs:770-777` (`git_out`), `:905-911` (`git_ok`)                                                                        |
| Layered config load, as `main.rs` does it                                                                | `crates/thegn-host/src/main.rs:1016-1021`                                                                                                         |
| `EXIT_RETRYABLE = 2`, the "not yet" exit code `session wait` already uses                                | `crates/thegn-host/src/cmd/mod.rs:70`; `crates/thegn-host/src/cmd/session.rs:391-393`                                                             |

### 1.1 The literal-brace bug is not thegn's

Item 3 asks that stage dispatch "tolerate LITERAL braces in issue bodies
(GraphQL `{ nodes { name } }` broke a naive substitution today)". It broke the
**Lead's** hand-rolled substitution. `agent_task::render` parses the _template_
once and then appends each variable's value to the output; a value is never fed
back through `parse` (`crates/thegn-core/src/agent_task.rs:302-321). So the moment
rendering moves server-side onto `render_prompt`, the class of bug disappears by
construction.

That is a property worth pinning rather than assuming: chunk 1 adds a regression
test that an `issue_body` of `query { nodes { name } }` renders verbatim, and
that an `{unclosed` body does not turn into `TemplateError::Unterminated`.

## 2. Decisions

### D1 — Stage dispatch is composed **in the CLI process**, not in the daemon

"Server-side" in the issue means _thegn-side rather than Lead-side_. It does not
have to mean _inside the pane daemon_, and it should not:

- the CLI process already has the layered config (`main.rs:1016-1021`) — the
  daemon has a possibly-stale boot snapshot (`daemon/mod.rs:293`), which is the
  very defect item 7 is about;
- the CLI process already opens the roster DB directly for every `dispatch`
  verb (`cmd/dispatch.rs:85`); adding a roster write inside the daemon would
  need new control-wire types, a new route, a `docs/api/control-v1.json`
  snapshot bump and three more surface projections, for zero capability;
- the only step that genuinely needs the daemon — spawning the session — is
  already a client call (`client.open`, `cmd/session.rs:289`).

So `session open --stage` is a composition in `cmd/session.rs` over
`sessions.open` + the local roster + the local filesystem. No new control
capability, no wire change.

### D2 — Only `done` is gated, and only for rows that carry an artifact

`dispatch set-status <id> done` is gated on the row's `artifact_path` existing
and being tracked by git. A row whose `artifact_path` is `NULL` is **not**
gated: plain (non-pipeline) dispatches predate stages entirely
(`issue.rs:252-257` documents the column as optional), and gating them would
break `set-status done` for every non-pipeline user with no failure it could
possibly catch. `failed`, `abandoned`, `merged` are never gated — a supervisor
must always be able to record a bad outcome.

Uncommitted changes in the worktree are **reported, never blocking**. The
tracked check already catches the pilot's real failure ("session exit ≠ done":
the worker wrote a file and never committed it), and a dirty tree is legitimate
mid-review. Report it and let the Lead judge.

### D3 — `dispatch verify` / `dispatch wait` are CLI-only catalog rows

Both read local git + the local roster (`verify`) or compose the routed
`sessions.wait` (`wait`). Neither wants an HTTP route. The catalog's existing
shape for exactly this is `search.query`/`search.replace`
(`capability.rs:596-608`): a row whose `surfaces` is `SurfaceSet::of(&[Surface::Cli])`,
declared in `cli_control_caps()` (`cmd/session.rs:505-561`). One catalog, no
`SURFACE_GAPS` excuse, no route.

`session close` needs **no** new row: `sessions.kill` already exists and is
already covered by the CLI surface through `API_CALLS` (`routes.rs:160`,
`cmd/session.rs:506-510`). This item is pure ergonomics — the `{"s": …}`
positional-key trap from the pilot notes is exactly the thing a named verb
removes.

### D4 — Item 7 is solved by a **per-request registry refresh**, not a reload verb

The issue offers either. Take the per-request read, at the one place that is
actually stale — agent resolution in `daemon/agent_open::resolve`
(`agent_open.rs:38-64`), which already runs on `spawn_blocking`
(`service.rs:415-430`). Reasons:

- a reload verb only helps a Lead that remembers to call it; a per-request read
  removes the failure mode instead of documenting a workaround;
- a reload verb needs `DaemonService.config: Arc<Config>` to become swappable
  across 12 read sites, plus a `Verb`, a catalog row, a route, a report type and
  a `control-v1.json` snapshot bump — a lot of surface for a problem the read
  closes outright.

**Only the registry is refreshed** — `agents`, `tools`, `pipeline` — merged over
the daemon's boot config. That is deliberate: the daemon may have been started
with `--set`/`--config` overrides (`main.rs:1016-1021` layers them), and a
wholesale re-load would silently discard them. A stage's `agent` name is the only
thing that goes stale in practice, so refresh exactly that and nothing else.

### D5 — Row-before-session ordering

`session open --stage` inserts the roster row _before_ it opens the session. If
the process dies between the two, the operator is left with a visible `queued`
row (re-drivable) rather than a live agent nobody has a record of. The row is
stamped with `session_id` + `artifact_path` and moved to `running` after the
open returns; if the open _fails_, the row is moved to `failed` and its id is
named in the error.

This needs one new roster write, `stamp_dispatch_run(id, session_id,
artifact_path)` — the roster has had no field update at all until now
(`store/notification.rs:152-171`). It stays a _store_ method, not a capability:
it is an internal step of a CLI verb, not an external door.

### D6 — Artifact paths are sanitized, per-issue, row-keyed

`.thegn/pipeline/<ISSUE>/<stage>/<row>.md`, where `<ISSUE>` is the roster
issue id with its `<provider>:` prefix stripped (`linear:THE-76` → `THE-76`).
Both `<ISSUE>` and `<stage>` are whitelist-sanitized to `[A-Za-z0-9._-]`,
because this string is joined under a worktree path and written to. `..`, `/`,
`\` and control characters must not survive — a tracker key is attacker-adjacent
data (an issue title is not, but an id from a misconfigured provider is cheap to
defend). The row id makes parallel coders of one stage collide-free.

## 3. The change, item by item

### Item 1 — native run-completion contract

_New pure policy_ (`thegn_core::pipeline_run`):

```rust
pub struct VerifyFacts { pub artifact: Option<String>, pub exists: bool, pub tracked: bool, pub dirty: bool }
pub struct VerifyReport { pub ok: bool, pub artifact: Option<String>, pub exists: bool,
                          pub tracked: bool, pub dirty: bool, pub reasons: Vec<String> }  // Serialize
pub fn verify_report(f: &VerifyFacts) -> VerifyReport
```

`ok = artifact.is_none() || (exists && tracked)`. `reasons` name each miss in
operator language ("artifact `X` does not exist under the worktree", "artifact
`X` exists but git does not track it — commit it"). `dirty` always reported.

_New CLI verb_ `thegn dispatch verify <row> [--json]` — gathers the facts from
the row's `worktree_path` and prints the report. Exit `0` when `ok`, `2`
(`EXIT_RETRYABLE`) when not, matching `session wait`'s "not yet" convention
(`cmd/session.rs:391-393`).

_Gate_ — `dispatch set-status <id> done` runs the same check and refuses with
the reasons unless `--force` is given. `--force` is recorded in the human/JSON
output so a forced completion is never invisible.

Fact gathering (host, `cmd/dispatch.rs`):

| fact      | how                                                                                                                             |
| --------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `exists`  | `Path::new(worktree).join(artifact).is_file()`                                                                                  |
| `tracked` | `util::git_ok(worktree, &["ls-files", "--error-unmatch", "--", artifact])`                                                      |
| `dirty`   | `util::git_out(worktree, &["status", "--porcelain"]).is_some()` (`git_out` returns `None` for empty output — `util.rs:770-777`) |

Note `--error-unmatch` also succeeds for a staged-but-uncommitted file. That is
the honest reading of "git knows about it"; the `dirty` flag is what
distinguishes staged from committed, which is why it is reported.

### Item 2 — `thegn session close <session>`

`client.kill(&session)` (`client.rs:238`). Human: `closed <id>`; `--json`:
`{"session":"…","closed":true}`. Degrades through the same `connect()` no-daemon
path as every other session verb (`cmd/session.rs:229-237`).

### Item 3 — server-side stage dispatch

```
thegn session open --stage <name> --issue <id> --worktree <path>
                   [--parent <row>] [--parent-artifact <path>] [--adopt] [--bind] [--json]
```

Steps, in order:

1. `cfg.pipeline.stage(name)` (`config_pipeline.rs:132`); on miss, error listing
   `stage_names()`.
2. Resolve the worktree to an absolute path (`cmd::resolve_worktree`,
   `cmd/session.rs:263`).
3. Resolve the branch: the registered worktree row, else
   `git rev-parse --abbrev-ref HEAD` — the same two-tier lookup
   `agent_open.rs:66-77` uses.
4. Bind issue facts. `{issue_number}` is the id with its provider prefix
   stripped; `{issue_title}` / `{issue_body}` / `{issue_url}` come from
   `client.issue_get(id)` (`client.rs:346`). If the tracker lookup fails **and**
   the stage's template references one of those three, fail with the tracker's
   own error — a prompt with a silently empty issue body is how a worker ends up
   implementing nothing. If the template references none of them, proceed. The
   "which variables does this template use" question is a new pure helper
   `agent_task::template_vars(&str) -> Result<Vec<String>, TemplateError>`
   over the existing `parse` (`agent_task.rs:238-281`).
5. Verify `--parent` exists (`db.get_dispatch(parent)?`), the same rule
   `cmd/dispatch.rs:111-119` enforces for `dispatch put`.
6. Insert the roster row (stage, parent, `agent_name = stage.agent`, status
   `queued`, no session, no artifact) → `row_id`.
7. `artifact = pipeline_run::artifact_path(&issue_id, stage_name, row_id)`.
8. `agent_task::render_prompt(&stage.prompt, &vars)` with the nine `STAGE_VARS`
   bound (`agent_task.rs:138-150`); `{parent_artifact}` from `--parent-artifact`,
   else the parent row's own `artifact_path`, else empty.
9. **Reject an empty rendered prompt** — mark the row `failed` and error. This is
   item 6's second half: an empty prompt silently produces an _interactive_
   session (`agent_open.rs:57-58`), which for a pipeline worker means a pane that
   sits there forever.
10. Seed permissions (item 4) when `stage.permissions` is non-empty.
11. `client.open(&spec)` with `headless: Some(true)`, `bind`, `adopt`. On error:
    row → `failed`, error names the row id.
12. `stamp_dispatch_run(row_id, session_id, artifact)` then status → `running`.
13. Print. `--json`:
    `{"row":12,"session":"…","stage":"architect","artifact":".thegn/pipeline/THE-76/architect/12.md","issue":"linear:THE-76","worktree":"/…","branch":"…"}`.

Constraints: `--stage` requires `--issue`; `--stage` conflicts with `--prompt`
(the prompt is the stage's); `--agent` becomes optional and defaults to
`stage.agent`, with an explicit `--agent` still winning (documented — a Lead
retrying a stage against a different harness should not have to edit config).

### Item 4 — `stage.permissions`

`PipelineStage` gains `permissions: Vec<String>` (default empty), documented in
`config/config.toml.example` beside the other stage keys (`:1505-1521`), and
validated in `validate_pipeline` (`config_pipeline.rs:204-250`): each entry
non-empty after trim, free of control characters, no duplicates — errors labelled
with the stage index like every other stage error.

thegn does **not** interpret the strings; they are Claude permission patterns
(`Bash(git status:*)`, `Read`, `mcp__x__y`) and belong to the harness. Say so in
the config docs.

Writing is a pure merge plus a file write:

```rust
pub fn merge_claude_allow(existing: Option<&str>, allow: &[String]) -> Result<String, PermsError>
```

- absent/blank existing ⇒ start from `{}`;
- existing JSON that is not an object, or `permissions` / `permissions.allow`
  present with the wrong type ⇒ `Err` — **never clobber a file we do not
  understand**;
- union into `permissions.allow`, existing order preserved, new entries
  appended, deduped;
- every other key in the file preserved verbatim; pretty-printed with a trailing
  newline.

Host side writes `<worktree>/.claude/settings.local.json`, creating `.claude/`.
Idempotent: re-dispatching the same stage produces a byte-identical file.

### Item 5 — `thegn dispatch wait`

```
thegn dispatch wait [--any | --row N] [--timeout <ms>] [--json]
```

_Pure_ candidate selection (`pipeline_run`):

```rust
pub struct WaitTarget { pub id: i64, pub session_id: String, pub stage: Option<String>, pub issue_id: String }
pub enum WaitSelectError { NoSuchRow(i64), NotActive(i64, &'static str), NoSession(i64), NoneActive }
pub fn wait_candidates(rows: &[AgentDispatch], row: Option<i64>) -> Result<Vec<WaitTarget>, WaitSelectError>
```

Candidates are rows with `status ∈ {Spawning, Running}` **and** a `session_id`.
`Queued` has no session yet; `WaitingHuman`/`PrOpen` are rows whose worker has
already finished (`issue.rs:376-382`) — including them would make `--any` return
instantly forever and starve the real wait. Default is `--any`; `--row` and
`--any` are mutually exclusive (clap `conflicts_with`).

Host: one `client.wait(sid, {"kind":"exited"}, timeout)` per target in a
`tokio::task::JoinSet`; the first to complete wins and dropping the set cancels
the rest. Prefer `JoinSet` over a `futures` combinator — tokio is already the
CLI's runtime (`cmd/session.rs:215`) and the cancellation semantics are the ones
we want. A dead session answers immediately from its tombstone
(`service.rs:764-784`), which is exactly the wake a supervisor wants when it
polls late. A target whose session is gone past the 10-minute tombstone TTL
(`tombstone.rs:41`) returns an error from the daemon: treat that as a wake with
`"gone": true` and `exit_code: null`, never as a failure of the whole call —
otherwise one reaped session makes `--any` unusable.

Output: `{"row":12,"session":"…","stage":"code","exit_code":0,"matched":true}`;
on timeout `{"matched":false}` and exit `2`.

### Item 6 — truthful `session list`

The data is already on the wire (`control/mod.rs:73-90`), it just is not printed.
`session_line` (`cmd/session.rs:144-162`, shared with `thegn attach`) gains a
state token: `live`, or `exited(<code>)` / `exited(?)` when unreapable, plus the
`final_state` word when the daemon has one. `session list --live` filters rows
with `exited_at_ms.is_some()`.

The `--prompt`-empty rejection is covered in item 3 step 9. It applies to
`--stage` and to an explicit `--headless` with a blank prompt; a plain
`session open --agent claude --worktree W` with no prompt stays an interactive
launch, because that is a real and correct use.

### Item 7 — daemon config freshness

New pure helper `pipeline_run::with_fresh_registry(base: &Config, fresh:
&Config) -> Config` — clone `base`, take `agents` / `tools` / `pipeline` from
`fresh`. In `agent_open::resolve`, load `Config::load_layered(&ProcessEnv, &[],
None)` (already on a blocking thread — `service.rs:415-430`) and apply it before
`command_for`. A failed load falls back to `base` unchanged: a config the daemon
cannot read must never turn a working dispatch into an error.

Cost: one TOML read per agent-open, on a path the module's own docs already
describe as "seconds, not milliseconds" (`agent_open.rs:26-28`). Not measurable.

## 4. Invariants this change must not break

- **0% idle** — every new verb is a one-shot CLI process. Nothing touches
  `run.rs`, the render path, or the loop. `dispatch wait` blocks in its _own_
  process on the daemon's event-driven wait (`service.rs:786-800`), which is
  already poll-free.
- **`thegn-core` stays substrate-free** — `pipeline_run` is pure: no tokio, no
  subprocess, no filesystem. Facts (git, fs) are gathered by the host and passed
  in. This is what keeps the 95% core coverage gate satisfiable.
- **One capability catalog** — two new CLI-only rows (`dispatches.verify`,
  `dispatches.wait`), each with a `Verb` and a `required_scope` fold entry
  (`control.rs:483` read side), each declared in `cli_control_caps()`. No
  `SURFACE_GAPS` entry: a CLI-only row is expressed by narrowing `surfaces`,
  never by an excuse (`docs/ARCHITECTURE.md` §6).
- **git is the source of truth** — the roster still stores a _pointer_; the
  verify gate literally asks git whether the artifact is real.
- **Ignored `Result`s** — the permissions write and the roster stamps are on the
  primary path of a user-invoked action; they must surface, not `let _ =`. Only
  genuinely best-effort work may be ignored, with a `// best-effort:` comment
  (`test/ignored-result-ratchet.txt`).
- **Config gates** — a new `[[pipeline.stages]]` key must appear in
  `config/config.toml.example` (`tests/config_example.rs`). `permissions` is a
  nested list-of-tables key, so `tests/env_overlay_coverage.rs` (shallow keys
  only) and `tests/hm_module_drift.rs` are unaffected — but re-run them.
- No new color/glyph literals, no new `#[cfg]` outside `platform/`, no
  `async fn` in a trait.

## 5. Chunking

Three chunks, **strictly serial**: 2 and 3 both edit
`crates/thegn-host/src/cmd/session.rs` and `test/smoke.sh`, so they are not
file-disjoint and must not be parallelized. 2 and 3 both depend on chunk 1's
core API.

| #   | Title                                                                              | Crate surface                                                                                                             | Depends on              |
| --- | ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| 1   | core policy, config, roster write, openspec                                        | `thegn-core`, `config/`, `openspec/`                                                                                      | —                       |
| 2   | run-completion contract (`dispatch verify` / `wait` / gated `done`) + catalog rows | `thegn-host/src/cmd/dispatch.rs`, `capability.rs`, `control.rs`, `cmd/session.rs::cli_control_caps` only, `test/smoke.sh` | 1                       |
| 3   | stage dispatch, `session close`, liveness, daemon registry refresh                 | `thegn-host/src/cmd/session.rs`, `daemon/agent_open.rs`, `test/smoke.sh`                                                  | 1 (and 2, for the file) |

The catalog rows live in chunk 2, with the verbs they describe, so no commit on
the branch ever claims a capability that does not exist.

Chunk files: `.thegn/pipeline/THE-76/code/chunk-1.md` … `chunk-3.md`.

## 6. Deliberately not doing

- **`thegn daemon reload-config`** — see D4. If a future need appears for
  reloading more than the registry (sandbox knobs, `[issues]` accounts), that is
  the moment to make `DaemonService.config` swappable and add the verb; doing it
  now buys nothing item 7 asks for.
- **A `dispatches.update` control capability** — the roster field stamp is an
  internal step of a CLI verb (D5). Exposing it externally would let a remote
  caller rewrite a row's artifact pointer, which is a durability hazard for zero
  demonstrated need.
- **Harness-generic permission seeding** — `.claude/settings.local.json` is
  Claude's file. A `[[harness]]`-shaped permissions seam is a real idea and a
  different change; today one harness has the problem.
- **Making the daemon own stage rendering** — see D1.
