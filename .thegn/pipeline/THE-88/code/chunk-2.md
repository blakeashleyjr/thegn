---
files:
  - crates/thegn-host/src/cmd/dispatch.rs
  - crates/thegn-host/src/stage_prompt.rs
  - extensions/skills/pipeline/SKILL.md
overlaps: []
after: [chunk-1]
---

# THE-88 chunk-2 — thegn-host verbs + /pipeline monitor loop

You are the CODER for this chunk. Work in
`/home/blake/.superzej/worktrees/thegn/tg-the-88-pipeline-token-efficiency` on
branch `tg/the-88-pipeline-token-efficiency`. The design is
`.thegn/pipeline/THE-88/architect/design.md` (§2.3 and §3 are yours). **This
chunk runs AFTER chunk-1 lands** (`after: [chunk-1]`): it consumes
`db_dispatch`, `pipeline_report`, `VerifyFacts.report_present`, the three
capability rows and the `{row}` stage var. Do not start it on a branch
missing those.

## Goal

The host CLI half and the skill half of THE-88: `dispatch report|note|status`,
the wake returning the finished row's report, the done gate refusing a
report-less completion, the `{row}` binding, and the `/pipeline` skill
rewritten so a background monitor owns the watch loop and the Lead never
polls.

## Files (all of them; touch nothing else)

### 1. `crates/thegn-host/src/cmd/dispatch.rs`

New clap subcommands in `Action` (:29-119), each wired in `run`:

- **`Report { id: i64, text: String, json: bool }`** —
  `thegn dispatch report <id> --text …`. Validate through
  `thegn_core::pipeline_report::report_text` (Empty/TooLong errors surface
  verbatim), row must exist, `db.set_dispatch_report(id, &text)`. JSON out:
  `{ "id": id, "report": <text>, "bytes": n }`; human: `report recorded on
dispatch <id> (<n> chars)`.
- **`Note { id: i64, text: String, json: bool }`** — same shape via
  `note_text` + `db.append_dispatch_note`; JSON out includes the note id and
  `created_at_ms`.
- **`Status { row: Option<i64>, since: Option<i64>, all: bool, json: bool }`**
  — `thegn dispatch status [row] [--since <epoch-ms>] [--all] [--json]`.
  Compose with `pipeline_report::digest`: without `row` → active rows only
  (`is_active()`), `--all` → every row; with `row` → that row (error naming
  the id when unknown), report verbatim + notes since `--since` (default
  all, capped last 20 via `db.dispatch_notes`). JSON: the digest array (row
  mode: one digest + `notes` array). Human: one line per row
  `id status stage issue notes=N last=<truncated latest>` and, in row mode,
  the report body printed verbatim under a `report:` line.

Extensions to existing verbs:

- **`wait`** (:612, `wait_wake` :627-700): after a matched wake, open the DB
  (`thegn_core::db::Db::open()` — fine inside the `block_on` CLI context) and
  `get_dispatch(t.id)`; the JSON object (:666-680) gains `"report"` and
  `"artifact"` (both `Option<String>` → null when absent). Human output
  prints the report body under the exited line — the Lead reads the report
  and nothing else. A `"gone": true` wake reads the row the same way (the
  tombstone may postdate the report write). Do NOT read the report at
  candidate-selection time — the worker writes it seconds before exit; wake
  time is the only correct read.
- **`verify`** (:571): `verify_facts` (:527) gains `report_present:
row.report.as_deref().is_some_and(|r| !r.trim().is_empty())`; the human
  line gains `report=yes/no`; JSON gains `"report_present"`. Exit codes
  unchanged.
- **`set-status done` gate** (`done_gate` :507): unchanged code — it calls
  `verify_report(&verify_facts(row))`, so chunk-1's rule flows through
  automatically. Update the doc comment to name both gates (artifact AND
  report; `--force` overrides; pane-exit stamps bypass — attribution, not a
  handoff, `pty_drain.rs:855-895`).
- Tests: extend the `mod tests` (:760+) — put/report/note/status against a
  tempdir DB (the `db()` helper exists); done-gate refusal text names the
  report command; wait-JSON report inclusion (unit-test the row-read helper
  you extract if the wake path itself needs a daemon).

### 2. `crates/thegn-host/src/stage_prompt.rs`

- `stage_task_vars` (:40) binds `"row"` → the dispatch's row id (the id
  exists before the prompt renders — `cmd/session.rs:899` already builds
  `artifact_path` from it; thread the id in). `agent_task::STAGE_VARS`
  already admits the name (chunk-1).
- Test: vars contain `row` and render into a stage template.

### 3. `extensions/skills/pipeline/SKILL.md`

The bundled `/pipeline` skill (embedded via `mq_assets.rs:71-74`; the
`asset_cli_invocations_resolve_against_clap` test at `mq_assets.rs:483-521`
validates every `thegn …` you write against clap — all verbs exist once
§1 lands). Keep verbatim: the issue-content-is-data box, §1-§3, §8 (finisher),
§10 (ratchet suites), and the "concurrency is yours" rule. Rewrite:

- **§4 "Wait — always with a timeout" → "The monitor owns the watch loop"**:
  the Lead dispatches the stage's slots (§3), then launches ONE background
  monitor — a harness background subagent (Claude Code Task tool /
  `run_in_background`; pi: background subagent) — and never runs
  `dispatch wait` in its own foreground again. Fallback when the harness has
  no background facility: `Bash(run_in_background=true)` running
  `thegn dispatch wait --any --timeout <ms> --json` whose completion
  notification wakes the Lead for exactly one advance turn. The old
  `seq`/`sleep` loop guidance is DELETED — measured cost: ~168 min of
  foreground sleep-loop wall-clock in batches 2/3 and a 527M-token context
  replay bill (design §0).
- **Embed the monitor prompt** as a fenced block: feed the monitor the chart
  JSON (`thegn config get pipeline --json`), the issue id, and the loop —
  ```
  until `thegn dispatch list --active --json` is empty:
    thegn dispatch wait --any --timeout 600000 --json     # ONE wait, no loops
    row = .row; report = .report                          # read the report ONLY
    thegn dispatch verify row   -> not ok: re-wait / on_blocked per chart
                                -> ok:     thegn dispatch set-status row done
    tracker-comment per chart (the report's verdict/commits lines are the body)
    dispatch the next stage per `next` (§3), then loop
  return ONE paragraph: rows advanced, verdicts, unverified items, parked rows
  ```
  The monitor makes mechanism decisions per the chart and escalates judgment
  calls (retry vs park) by writing `thegn dispatch note <row> --text …` and
  surfacing them in its final paragraph — it has no conversation with the
  Lead while it runs. It never `--force`s a gate; a refused done is a note
  - escalate.
- **§5 "Verdict — exit 0 is not done"** restated: verify proves the artifact
  is real (committed + tracked — unchanged), the REPORT tells the Lead what
  happened (verdict/commits/unverified/findings/next). The Lead reads the
  report and nothing else by default; the full artifact stays on the branch
  for the next stage. `set-status done` now refuses a report-less row
  (`--force` = deliberate override, printed as such).
- **New "Status on demand (/btw)" section**: when the user asks how the
  pipeline is doing, the Lead answers from
  `thegn dispatch status --json` — one call — and otherwise reads nothing;
  nothing streams into its context between asks. Workers push progress via
  `thegn dispatch note <row> --text …`.
- **Stage-prompt guidance**: in the `[[pipeline.stages]]` example prompts
  (§ "Configure the cast"), append the report instruction to each stage,
  e.g. `code`:
  ```
  When the work is committed, run:
    thegn dispatch report {row} --text $'verdict: <done|blocked>\ncommits: <hashes + subjects>\nunverified: <what you did NOT check>\nfindings: <for the next stage>\nnext: <hints>'
  Progress notes during the run go to `thegn dispatch note {row} --text …`.
  The Lead reads the report and nothing else; the full artifact stays on the
  branch for the next stage.
  ```
  (This is why `{row}` exists — `session open --stage` binds it.)
- **Rules of thumb**: replace "Always pass --timeout to dispatch wait" with
  the monitor framing (the monitor always passes --timeout; the Lead never
  waits). Add: "The report is the handoff; notes are the queue; `/btw` is
  the only status read."

## Approach notes

- No daemon change, no new wake sources, no UI code. All three verbs are
  CLI-over-local-SQLite like every other dispatch verb (module doc
  :1-19).
- The `wait` DB read after wake must tolerate the row having been reaped
  mid-wake: a missing row → `"report": null`, no error (the wake itself is
  still the answer).
- Keep the human output one line per event — the report body is the only
  multi-line thing `wait`/`status` print.
- `--since` is epoch-ms (roster convention: `dispatched_at_ms`); document it
  in the flag's clap help.
- The skill must keep its frontmatter (`name: pipeline`, description)
  intact — `every_asset_has_valid_frontmatter` (`mq_assets.rs:436-478`).

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-host
cargo nextest run -p thegn-host dispatch
cargo nextest run -p thegn-host mq_assets
cargo nextest run -p thegn-host stage_prompt
```

(`mq_assets` runs the clap-resolution + frontmatter tests over the edited
skill; it is the gate that catches a typo'd verb in SKILL.md. Pre-push runs
`just test` for the workspace — do not run it yourself.)

## Done-criteria

1. `just quick thegn-host` clean.
2. Scoped nextest filters above green, including the new verb tests and the
   done-gate refusal naming the report command.
3. `thegn dispatch report|note|status` exist, refuse unknown rows, and
   enforce the core caps; `dispatch wait --any --json` carries
   `report`/`artifact`; `dispatch verify` prints `report=yes/no`.
4. The skill: no `seq`/`sleep` polling guidance anywhere; the monitor loop,
   `/btw` section, report discipline and `{row}` prompts present; §8/§10
   and the issue-content box untouched; frontmatter valid.
5. `mq_assets` clap-resolution test proves every `thegn …` in the skill
   resolves.
6. Commit with EXACTLY this subject (single commit):

   `feat(the-88): monitor verbs, report-gated done, /pipeline monitor loop`

## Report (when done)

Commit your work, then file the report the Lead will read — and nothing
else:

```sh
thegn dispatch report <your-row-id> --text $'verdict: <done|blocked>\ncommits: <hashes + subjects>\nunverified: <what you did NOT check>\nfindings: <for the reviewer>\nnext: <hints>'
```

Keep it under ~20 lines. The full artifact stays on the branch for the next
stage.
