# Chunk 1 done — Finisher/resume: `thegn session open --resume-work <row-id>`

Commits on `tg/the-86-pipeline-v3`:

- `f9710b65` — `feat(pipeline): pure finisher-prompt composition module (THE-86 chunk 1)` (incremental: core + lib.rs line)
- `9aa9d9c6` — `feat(pipeline): session open --resume-work composes the finisher dispatch (THE-86)` (exact subject from the spec; host + docs + smoke)

## What was implemented

- **`crates/thegn-core/src/pipeline_resume.rs`** (NEW) — pure finisher-prompt
  composition: `FinisherInput`, `SCREEN_TAIL_LINES = 8`, `finisher_prompt`.
  Module header restates the `pipeline_run` doctrine (no I/O, no subprocess,
  no tokio, no clock/env/randomness). All three artifact-state sentences are
  verbatim per spec; `git_status`/`diff_stat` render in fenced blocks only
  when non-empty; the screen tail is quoted `| `-prefixed, truncated to the
  last 8 non-blank lines, with an explicit "screen is unavailable" sentence
  when empty; the closer carries the exit-0-is-not-done rule and the commit
  instruction. Embedded text is sanitized: ANSI/CSI/OSC escapes and control
  chars are stripped (newlines/tabs survive) so a hostile tracker body cannot
  smuggle terminal sequences into the next worker's context.
  11 in-module tests.
- **`crates/thegn-core/src/lib.rs`** — one `pub mod pipeline_resume;` line
  (alphabetical, before `pipeline_run`), staged as its own hunk.
- **`crates/thegn-host/src/cmd/session.rs`**:
  - `Open` gains `--resume-work <row-id>` with the specified
    `conflicts_with_all` (`stage, agent, issue, prompt, parent,
parent_artifact, worktree`); verified live that each conflict fires.
  - Factored out of `open_stage` behavior-neutrally: `resolve_branch(db, wt)`
    (registry-then-`git rev-parse`) and `gather_issue_facts(client, stage,
issue)` (the `template_vars`/`needs_tracker` block). `open_stage` calls
    both; its existing tests pass unchanged.
  - `resume_row_checks(row_id, row)` — the pure offline row wording (row miss
    → `no dispatch with id {id}`; blank/missing stage → `dispatch {id} is not
a pipeline row (no stage) — --resume-work resumes a --stage dispatch`);
    `resume_preflight(cfg, db, row_id)` runs those + `stage_or_bail` in the
    same pre-`connect` slot as `open_preflight` (doc-comment extended).
    4 unit tests over the wording.
  - `resume_work(cfg, client, row_id, bind, adopt, json)`: row is the record
    (agent = `row.agent_name`; stage/issue/worktree reused; new row parented
    on it); re-render via `stage_task_vars` with `artifact =
row.artifact_path or ""` and `{parent_artifact}` from the parent row;
    empty-render refusal (same message as `open_stage`); finisher facts via
    `dispatch::verify_facts(&row)` + `git status --porcelain` / `git diff
--stat` (`""` on failure) + `client.snapshot` → `snapshot_text` →
    non-blank tail (ANY failure → empty tail); `pipeline_resume::finisher_prompt`;
    row-before-open (D5) with `artifact = pipeline_run::artifact_path(issue,
stage, new_row)`; open via the same `OpenSpec`/`AgentLaunch` shape
    (`headless: Some(true)`, `stage: Some(...)`, `continue_last: false` — a
    resume opens a NEW session; the harness-native continue is the daemon
    retry path's mechanism); success stamps + `Running` + prints
    (`--json`: `{row, session, stage, artifact, issue, worktree,
resumed_from}`; human: `dispatch {n} → session {s} (stage {x}, resume of
{row_id})`); failure after insert marks the row `failed` (best-effort)
    and wraps with `dispatch {n} failed`.
- **`crates/thegn-host/src/cmd/dispatch.rs`** — `fn verify_facts` →
  `pub(crate) fn verify_facts` (+ doc note). Note: the change is present at
  line 366 and landed in the branch history via chunk 2's commit `b8121cb7`
  (shared-worktree staging overlap), not my commit — content is exactly as
  specified.
- **`docs/cli.md`** — one line after the grammar table's Control-plane row:
  `session open --resume-work <row>` resumes a failed pipeline row through
  the roster.
- **`test/smoke.sh`** — two no-daemon checks appended after the THE-76 block:
  unknown row refused offline naming `999999` (asserted the no-daemon message
  does NOT appear); `dispatch put linear:SMOKE-5 …` (plain row) then
  `--resume-work <id>` refused matching `not a pipeline row`.

## Done criteria

- [x] `just quick thegn-core` and `just quick thegn-host` clean (clippy -D
      warnings; final runs green at HEAD `9aa9d9c6` + chunk-2 siblings).
- [x] `cargo nextest run -p thegn-core pipeline_resume` — 11 passed.
- [x] `cargo nextest run -p thegn-host session::` — 63 passed (incl. the 4
      new `resume_work_tests`); `catalog_tests` green (no catalog drift —
      the flag rides `sessions.open`).
- [x] The two new smoke checks pass against the real binary in an isolated
      `XDG_STATE_HOME` (the exact commands smoke.sh issues); both exit
      non-zero with the specified wording, daemon-free.
- [x] `--resume-work` appears in `session open --help`; conflicts verified
      live against `--stage`, `--agent`, `--worktree`.
- [x] Commit subject exact (see above).

## Unverified / deviations

- **Full `bash test/smoke.sh` not run** (pre-push gate; its daemon section is
  chunk-adjacent and the heavy-build policy forbids the full run here). Only
  the two new checks were executed standalone. The rest of the suite is
  unverified in this worktree.
- **Deliberate clap deviation the spec implied but did not spell out:** with
  the specified conflicts, `--worktree`/`--agent` had to stop being
  unconditionally required, or clap would refuse `session open --resume-work
999999` with a usage error before the offline row refusal could ever fire
  (breaking the chunk's own smoke check #1). `worktree` became
  `Option<String>` + `required_unless_present = "resume_work"`, and `agent`'s
  requirement became `required_unless_present_any = ["stage", "resume_work"]`.
  Verified in a standalone clap 4.6 repro that (a) `required = false` +
  `required_unless_present` on a non-Option field does NOT relax, (b) the
  `Option` form works, (c) the plain paths still demand `--agent`/`--worktree`
  exactly as before (regression-checked live).
- `resume_work` opens its own `Db` inside the fn rather than taking a `db`
  param (the spec's signature sketch); behavior equivalent.
- `verify_facts` sits at line 366 in the final tree, not ~363 as the chunk
  cited (pre-existing drift; line cites were against an earlier HEAD).
- Chunk 2 landed concurrently on this branch (helper move to
  `stage_prompt.rs`, `AgentLaunch.continue_last`, db v59). Integration
  touches I made inside my file: added `continue_last: false` to the three
  `AgentLaunch` initializers in `session.rs`, deduped a double-inserted
  field, dropped a now-unused `TaskVars` import, converted an orphaned doc
  comment left by the move into a plain comment. `session open`'s other
  semantics untouched.
- e2e (`just e2e`) not run, per instructions.
