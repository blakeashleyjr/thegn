# Chunk 1 — Finisher/resume: `thegn session open --resume-work <row-id>`

Design: `.thegn/pipeline/THE-86/architect/design.md` §1. Serial order **1 → 2 → 3**
(this chunk first; chunk 2's `stage_prompt.rs` helper move lands on top of the
factoring this chunk does).

## Files touched (exact paths)

| File                                       | Change                                                                                                                                                         |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/pipeline_resume.rs` | **NEW** — pure finisher-prompt composition                                                                                                                     |
| `crates/thegn-core/src/lib.rs`             | one `pub mod pipeline_resume;` line (keep alphabetical)                                                                                                        |
| `crates/thegn-host/src/cmd/session.rs`     | `--resume-work` flag on `SessionAction::Open`; `resume_work()` fn; factor `gather_issue_facts` + `resolve_branch` out of `open_stage` (reused, not duplicated) |
| `crates/thegn-host/src/cmd/dispatch.rs`    | one line: `fn verify_facts` → `pub(crate) fn verify_facts` (line ~363)                                                                                         |
| `docs/cli.md`                              | one line in the Control plane paragraph: `session open --resume-work <row>` resumes a failed pipeline row through the roster                                   |
| `test/smoke.sh`                            | two no-daemon checks appended after the THE-76 block (~line 1205)                                                                                              |

Nothing else. No control-wire change, no catalog row (the flag rides the
existing `sessions.open` verb; `cli_control_verbs_cover_catalog` must stay green
untouched), no db change, no config change.

## Approach

1. **Core, pure** (`pipeline_resume.rs`, module header restates the
   `pipeline_run.rs` doctrine: no I/O, no subprocess, no tokio):

   ```rust
   pub struct FinisherInput<'a> {
       pub stage_name: &'a str,
       pub stage_prompt: &'a str,   // the RENDERED original task
       pub artifact: &'a str,
       pub artifact_exists: bool,
       pub artifact_tracked: bool,
       pub git_status: &'a str,     // "" when clean
       pub diff_stat: &'a str,      // "" when empty
       pub screen_tail: &'a [String],
   }
   pub const SCREEN_TAIL_LINES: usize = 8;
   pub fn finisher_prompt(i: &FinisherInput) -> String;
   ```

   Content rules: names the stage and issue context; embeds the rendered stage
   prompt verbatim; artifact-state paragraph exactly one of —
   - `exists=false` → "the handoff artifact `<path>` was never written";
   - `exists && !tracked` → "written but NOT committed — committing it is part
     of finishing";
   - `exists && tracked` → "already committed — verify it is current before
     declaring the stage done".
     `git_status`/`diff_stat` render in fenced blocks **only when non-empty**;
     the screen tail is quoted line-by-line (prefix `| `), truncated to the last
     `SCREEN_TAIL_LINES` non-blank lines (the caller may pass more; this fn
     truncates); closer carries the **exit-0-is-not-done** rule and the commit
     instruction. Deterministic: no clock, no randomness, no env.

2. **Host** (`cmd/session.rs`):
   - clap: on `Open`, add
     `#[arg(long, conflicts_with_all = ["stage", "agent", "issue", "prompt", "parent", "parent_artifact", "worktree"])] resume_work: Option<i64>`.
     (`--bind`/`--adopt`/`--json` remain legal.)
   - Factor the two reusable pieces out of `open_stage` **as-is** (no behavior
     change, `open_stage` calls them): `gather_issue_facts(cfg, client, stage,
issue) -> Result<IssueFacts>` (the `template_vars`/`needs_tracker` block,
     `cmd/session.rs:788-818`) and `resolve_branch(db-less: registry then
`git rev-parse`, `cmd/session.rs:770-786`).
   - `resume_work(cfg, client, db, row_id, bind, adopt, json)`: 1. offline refusals **before `connect`**: `db.get_dispatch(row_id)` miss →
     `no dispatch with id {row_id}` (same wording `dispatch set-status`
     uses); `row.stage` `None`/blank → `dispatch {id} is not a pipeline row
(no stage) — --resume-work resumes a --stage dispatch`; then
     `stage_or_bail(cfg, stage)` (the same listing-on-miss message,
     `cmd/session.rs:655`). 2. `--agent` override: agent = explicit `--agent` … but clap conflicts it;
     instead accept `agent` via the row only (keep the conflicts list as
     specified — the row's agent is the record; harness changes go through
     config or a later chunk). Final: agent = `row.agent_name`. 3. render the stage template with `stage_task_vars` (issue facts, branch,
     worktree = `row.worktree_path`, stage, artifact = `row.artifact_path`
     or `""`, parent_artifact = parent row's artifact via
     `db.get_dispatch(row.parent_id)`, `""` when none) + empty-render
     refusal (same message as `open_stage`, `cmd/session.rs:839`). 4. finisher facts: `dispatch::verify_facts(&row)` (now `pub(crate)`);
     `git_out(wt, ["status","--porcelain"])` / `git_out(wt, ["diff","--stat"])`
     (`""` on None); screen tail: after `connect`,
     `client.snapshot(row.session_id)` → `snapshot_text` (already
     `pub(crate)`, `cmd/session.rs:911`) → keep non-blank lines; ANY error
     (no daemon tombstone, session never had a screen) → empty tail (never a
     hard failure — the prompt simply says the previous screen is
     unavailable when the tail is empty). 5. compose with `pipeline_resume::finisher_prompt`. 6. **row before open (D5)**: `put_agent_dispatch(NewDispatch { issue_id:
row.issue_id, worktree_path: &row.worktree_path, agent_name: &agent,
stage: row.stage.as_deref(), parent_id: Some(row.id), session_id: None,
artifact_path: None })`; `artifact =
pipeline_run::artifact_path(&row.issue_id, stage.name, new_row_id)`. 7. open via the same `OpenSpec`/`AgentLaunch` shape `open_stage` builds
     (`headless: Some(true)`, `stage: Some(stage.name.clone())` — the stage
     overrides layer exactly as a fresh dispatch). Success:
     `stamp_dispatch_run(new_row, &info.id, &artifact)` + `Running` + print
     (`json`: `{row, session, stage, artifact, issue, worktree,
resumed_from: row_id}`; human: `dispatch {n} → session {s} (stage {x},
resume of {row_id})`). Failure after insert: mark the row `failed`
     (best-effort, the `open_stage` Err arm idiom, `cmd/session.rs:866-874`)
     and wrap the error with `dispatch {new_row} failed`.
   - Route the clap arm; extend `open_preflight`'s doc-comment list.

3. **docs + smoke**: the cli.md line; two smoke checks appended (both
   daemon-free, before any `session open` that needs a daemon):
   - `session open --resume-work 999999` exits non-zero naming `999999`
     (offline row lookup — no "no thegn pane daemon" in the message);
   - `dispatch put linear:SMOKE-5 '$R' claude` (plain row, no `--stage`) then
     `session open --resume-work <that id>` exits non-zero matching
     `not a pipeline row`.

## Tests

Scoped while iterating (no full-workspace builds):

```sh
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core pipeline_resume
cargo nextest run -p thegn-host session::  # existing open_stage/session tests stay green
```

- **core** (`pipeline_resume.rs` `#[cfg(test)]`): all three artifact-state
  branches produce their exact sentence; status/diff blocks present iff
  non-empty; tail truncation to `SCREEN_TAIL_LINES` non-blank lines; empty
  everything still renders (and says the screen is unavailable); deterministic
  (two calls equal); no ANSI/control chars pass through unflagged.
- **host**: `open_stage`'s existing tests must pass unchanged (the factoring is
  behavior-neutral); a unit test for the offline refusal wording of
  `resume_work`'s row checks where testable without a client (split the
  pure checks into a helper fn if needed).

## Done criteria

- [ ] `just quick thegn-core && just quick thegn-host` clean.
- [ ] `cargo nextest run -p thegn-core pipeline_resume` and `-p thegn-host
session::` green; `cargo nextest run -p thegn-host catalog_tests` still
      green (no catalog drift).
- [ ] `bash test/smoke.sh` section touched runs clean (or full `just smoke` if
      the harness allows; the two new checks pass).
- [ ] `--resume-work` appears in `session open --help` with the conflicts.
- [ ] Commit subject EXACTLY:
      `feat(pipeline): session open --resume-work composes the finisher dispatch (THE-86)`
