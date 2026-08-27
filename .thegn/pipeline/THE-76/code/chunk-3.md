# THE-76 chunk 3 — stage dispatch, `session close`, truthful liveness, daemon registry freshness

**Runs:** AFTER chunks 1 and 2.
**Overlap:** shares `crates/thegn-host/src/cmd/session.rs` and `test/smoke.sh`
with chunk 2 — **serial only.** Chunk 2 owns `cli_control_caps()` at the bottom
of `session.rs` (`:505-561`); leave that function alone. You own
`SessionAction`, `run_async`, and `session_line`.
**Read first:** `.thegn/pipeline/THE-76/architect/design.md` §2 D1/D4/D5/D6, §3
items 2, 3, 4, 6, 7.

## Files touched (exact)

1. `crates/thegn-host/src/cmd/session.rs` — `close`, `list --live`, `session_line`, `open --stage`
2. `crates/thegn-host/src/daemon/agent_open.rs` — per-request registry refresh
3. `test/smoke.sh` — session coverage

## 1. `thegn session close <session> [--json]`

New `SessionAction::Close { session: String, json: bool }`. Body: `client.kill(&session).await?`
(`thegn-svc/src/control/client.rs:238`). Human: `closed <id>`; `--json`:
`{"session":"…","closed":true}` — and add the `Close { json: true, .. }` arm to
the `json_mode` match at `cmd/session.rs:220-228` so the no-daemon degradation
emits `{"error":"no_daemon"}` like every other JSON verb.

**No catalog change.** `sessions.kill` is already routed
(`thegn-svc/src/control/routes.rs:63,160`) and already covered on the CLI surface
through `API_CALLS` (`cmd/session.rs:506-510`). This verb exists purely to retire
`thegn api call sessions.kill --params '{"s":…}'`, whose positional key is a
documented foot-gun.

## 2. Truthful `session list`

`session_line` (`cmd/session.rs:144-162`, shared with `thegn attach`'s no-arg
listing) gains a liveness token. The data is already on the wire and has been all
along (`thegn-svc/src/control/mod.rs:73-90`; the daemon lists tombstones at
`daemon/service.rs:398-410`) — it simply was never printed, so a supervisor read
a stale pid and concluded the worker was alive.

- `s.exited_at_ms.is_none()` ⇒ `live`
- else `exited(<code>)`, or `exited(?)` when `exit_code` is `None` (a killed or
  unreapable child — `daemon/tombstone.rs:52-54`), suffixed with the
  `final_state` word when present, e.g. `exited(0,done)`.

Put the token immediately after the id so a `grep` on a fixed column works, and
keep the rest of the line unchanged (`thegn attach`'s listing shares it).

Add `#[arg(long)] live: bool` to `SessionAction::List` and filter out rows with
`exited_at_ms.is_some()` when set. With `--json`, filter the vector before
serializing — a `--live --json` caller must not have to re-filter.

Unit-test the formatting in the existing test module: a live row, an exited row
with a code, an exited row without one. Assert the token, not the whole line.

## 3. `session open --stage` — the one-call dispatch

New flags on `SessionAction::Open` (`cmd/session.rs:26-56`):

```rust
/// Dispatch a [[pipeline.stages]] step: render its prompt, seed its
/// permissions, open the session and write the roster row, in one call.
#[arg(long)] stage: Option<String>,
/// Tracker issue id in roster form (`linear:THE-76`). Required with --stage.
#[arg(long)] issue: Option<String>,
/// The roster row this one was chunked out of.
#[arg(long)] parent: Option<i64>,
/// Override the parent's handoff artifact for {parent_artifact}.
#[arg(long)] parent_artifact: Option<String>,
```

- `--agent` becomes `Option<String>`: required without `--stage`, defaulting to
  `stage.agent` with `--stage`. An explicit `--agent` still wins (a Lead retrying
  a stage on a different harness should not have to edit config) — document it in
  the flag help.
- `--stage` `conflicts_with = "prompt"` — the prompt comes from the template.
- `--stage` `requires = "issue"`.

Keep the existing non-stage path byte-identical. Put the stage path in a new
private `async fn open_stage(...)` in the same file so the `run_async` match arm
stays readable.

### Order of operations (do not reorder)

1. `let stage = cfg.pipeline.stage(name)` (`config_pipeline.rs:132`); on miss,
   `bail!` listing `cfg.pipeline.stage_names()`.
2. Resolve the worktree to an absolute path with `crate::cmd::resolve_worktree`
   (`cmd/session.rs:263`).
3. Branch: the registered worktree row, else
   `util::git_out(&wt, &["rev-parse", "--abbrev-ref", "HEAD"])` — the two-tier
   lookup `daemon/agent_open.rs:66-77` already uses. Empty is acceptable.
4. Issue facts. `{issue_number}` is `pipeline_run::issue_key(&issue_id)`.
   `{issue_title}` / `{issue_body}` / `{issue_url}` come from
   `client.issue_get(&issue_id)` (`client.rs:346`). Use
   `agent_task::template_vars(&stage.prompt)?` (chunk 1) to decide:
   - the template references **none** of the three ⇒ skip the lookup entirely
     (bind empty strings) — a stage that doesn't read the issue must not require
     a configured tracker;
   - it references any of them and the lookup fails ⇒ `bail!` with the tracker's
     own error. A prompt with a silently empty issue body is how a worker ends up
     implementing nothing.
5. `--parent`: `db.get_dispatch(parent)?` must be `Some`, else `bail!` naming the
   id — the same rule `dispatch put` enforces (`cmd/dispatch.rs:111-119`). Do not
   reach into `cmd::dispatch`; two lines here keep the chunks uncoupled.
6. **Insert the roster row before opening the session** —
   `db.put_agent_dispatch(NewDispatch { issue_id, worktree_path, agent_name,
stage: Some(name), parent_id, session_id: None, artifact_path: None })` →
   `row_id`. If the process dies next, the operator is left with a visible
   `queued` row rather than a live agent nobody has a record of.
7. `let artifact = pipeline_run::artifact_path(&issue_id, stage_name, row_id);`
8. Render: `agent_task::render_prompt(&stage.prompt, &vars)` with all nine
   `STAGE_VARS` bound (`agent_task.rs:138-150`) — `stage`, `artifact`,
   `parent_artifact` (from `--parent-artifact`, else the parent row's
   `artifact_path`, else `""`), plus the six issue/branch/worktree ones. **Bind
   every variable**, even the ones the template does not use: `render` errors on
   an unbound name (`agent_task.rs:308-312`).
   Literal braces in the issue body are safe by construction — values are
   substituted into the output and never re-parsed (`agent_task.rs:302-321`);
   chunk 1 pins that with a test. Do not add your own escaping.
9. **Reject an empty rendered prompt** (trim): set the row `failed` and `bail!`.
   An empty prompt silently opens an _interactive_ session
   (`daemon/agent_open.rs:57-58`), i.e. a pipeline worker that sits there
   forever. Apply the same rule to a non-stage `--headless` with a blank
   `--prompt`; leave a plain promptless `session open` interactive, because that
   is a real and correct use.
10. Seed permissions when `stage.permissions` is non-empty:
    read `<wt>/.claude/settings.local.json` if present,
    `pipeline_run::merge_claude_allow(existing.as_deref(), &stage.permissions)?`,
    `create_dir_all(<wt>/.claude)`, write the returned text. Surface any error
    (`?`) — this is the primary path of a user-invoked action, not best-effort;
    a worker that launches without its permissions is the pilot's
    "headless claude auto-denies everything" failure.
11. `client.open(&spec)` with `agent`, the rendered `prompt`, `headless: Some(true)`,
    `bind_worktree: bind`, `resume: None`, and `adopt` — the same `OpenSpec`
    construction as the existing arm (`cmd/session.rs:260-288`).
    **On error:** `db.update_dispatch_status(row_id, AgentDispatchStatus::Failed)`,
    then return the error with the row id in the message.
12. `db.stamp_dispatch_run(row_id, &info.id, &artifact)?` (chunk 1) then
    `db.update_dispatch_status(row_id, AgentDispatchStatus::Running)?`.
13. Output — `--json`:
    `{"row":12,"session":"…","stage":"architect","artifact":".thegn/pipeline/THE-76/architect/12.md","issue":"linear:THE-76","worktree":"/…","branch":"…","agent":"…"}`;
    human: `dispatch 12 → running (stage architect, session <id>, artifact <path>)`.
    The `--json` shape is what the Lead parses — keep it flat and stable.

## 4. Daemon registry freshness (item 7)

In `daemon/agent_open::resolve` (`daemon/agent_open.rs:38-64`), before
`command_for`, refresh the agent registry:

```rust
// The daemon holds a BOOT snapshot of config (`daemon/mod.rs:293`), so an
// `[[agents]]` entry added since launch was invisible until a restart. Re-read
// the layered config here — we are already on a blocking thread
// (`daemon/service.rs:415-430`) and this path is documented as seconds, not
// milliseconds (`agent_open.rs:26-28`) — and take ONLY the registries from it:
// the daemon may have booted with `--set`/`--config` overrides, and a wholesale
// swap would silently discard them.
let fresh = thegn_core::config::Config::load_layered(
    &thegn_core::config::ProcessEnv, &[], None);
let cfg = &thegn_core::pipeline_run::with_fresh_registry(cfg, &fresh);
```

Keep it total: if the load produces a default/empty config, the merged result is
a config with no agents — guard by only applying the merge when
`!fresh.agents.is_empty() || !fresh.tools.is_empty() || !fresh.pipeline.stages.is_empty()`,
and fall back to `cfg` otherwise. A config the daemon cannot read must never turn
a working dispatch into an error.

Add a test in `agent_open`'s test module (or a new one) covering the merge
decision — the fs read itself is a seam and does not need a test; the "empty
fresh config falls back" branch does.

## 5. `test/smoke.sh`

Extend the session block (`test/smoke.sh:1062-1131`), same `check` style:

- `session close bogus` without a daemon exits 1 with the clear message (it
  shares the `connect` path — `cmd/session.rs:195-210`).
- `session list --live` without a daemon exits 1, and `--live --json` emits
  `{"error":"no_daemon"}`.
- `session open --stage nosuchstage --issue linear:SMOKE-1 --worktree "$R"`
  fails naming the stage, **without** needing a daemon (the config lookup is
  step 1, before `connect`) — if your implementation connects first, reorder so
  the config error is reported offline.
- `session open --stage X --prompt Y` is refused by clap (`conflicts_with`).
- `session open --agent claude --worktree "$R" --headless` with no prompt is
  refused with the empty-prompt message.

Keep every check offline; do not start a daemon in this section.

## Tests to run (scoped)

```sh
just quick thegn-host
cargo nextest run -p thegn-host session
cargo nextest run -p thegn-host agent_open
shellcheck test/smoke.sh
```

Do **not** run `just test`, `just ci`, `just coverage`, `just smoke`, or e2e, and
do not start any full-workspace compile.

## Done criteria

- [ ] `thegn session close <id>` works and degrades cleanly with no daemon.
- [ ] `session list` prints a liveness token for every row and `--live` filters
      exited ones in both human and JSON mode, with unit tests on the formatter.
- [ ] `session open --stage … --issue …` performs the whole dispatch in one call:
      stage lookup → roster row → artifact path → rendered prompt → permissions →
      session → stamp → `running`, with the failure paths leaving the row
      `failed` (never `queued`, never orphaned).
- [ ] An empty rendered prompt, and an explicit `--headless` with a blank
      prompt, are refused.
- [ ] An issue body containing `{ nodes { name } }` dispatches successfully with
      the braces intact in the prompt (chunk 1 pins the engine property; assert
      the end-to-end path here with a unit test over the render step).
- [ ] `stage.permissions` are seeded into `<worktree>/.claude/settings.local.json`
      without destroying pre-existing keys; re-dispatch is byte-idempotent.
- [ ] A newly added `[[agents]]` entry is visible to a _running_ daemon with no
      restart, and a daemon booted with `--set` overrides keeps them.
- [ ] The existing non-stage `session open` behaviour is unchanged.
- [ ] No new `let _ =` / `.ok()` without a `// best-effort:` reason.
- [ ] Scoped tests above are green.

**Commit subject (exact):**

```
feat(session): server-side stage dispatch, close, and truthful liveness (THE-76)
```

Also write your summary to the artifact path your roster row carries and commit
it in the same commit.
