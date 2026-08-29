# THE-88 — Pipeline token efficiency: the Lead reads only the report

Architect design. Branch `tg/the-88-pipeline-token-efficiency`.
Evidence rule honored: every claim below cites file:line or a measured number;
nothing is a hypothesis.

---

## 0. Measured first — the audit

**Sources.** The dispatch roster (`$XDG_STATE_HOME/thegn/thegn.db`,
`agent_dispatches`, rows 29–147 = batches 2+3, 119 rows, 10 lane issues) and
the Lead's Claude Code transcripts in
`~/.claude/projects/-home-blake-code-thegn/`:
`74e3aaa1…jsonl` (Lead A, 12.4 MB, 2026-08-25T07:04Z → 08-27T14:58Z — batch 1
plus context) and `2581c8d6…jsonl` (Lead B, 5.8 MB, 2026-08-27T15:43Z →
08-29T01:43Z — batches 2+3). Batch windows are cut on roster
`dispatched_at_ms`: batch 2 = rows 29–102, batch 3 = rows 103–147. Token
estimates are chars/4 for tool I/O; "real" numbers are the per-message
`usage` fields the transcripts already carry. (Caveat: batch 2's first seven
`dispatch put`s, rows 29–36 at 15:06–15:08Z, predate Lead B's first record by
35 min and are not in the batch-2 window below; the sustained polling behavior
that defines the batch is.)

### Per-activity breakdown, Lead B (batches 2/3)

| activity                    | batch 2                                 | batch 3            | notes                                                      |
| --------------------------- | --------------------------------------- | ------------------ | ---------------------------------------------------------- |
| assistant turns             | 795                                     | 244                | every turn replays the context                             |
| real output tokens          | 1,153,753                               | 344,144            | Lead's own generation                                      |
| context replay (cache_read) | 336,837,040                             | 190,492,523        | **the multiplier**                                         |
| poll-loop Bash calls        | 24 (4 fused advance+wait, 20 poll-only) | 4                  | `for i in $(seq 1 N); do sleep 30; …done` foreground turns |
| poll foreground wall-clock  | ~152 min                                | ~16 min            | time the Lead was unusable                                 |
| poll tool I/O               | ~46K tok in / ~20K out                  | ~32K / ~9K         | small!                                                     |
| roster advance calls        | 42                                      | 35                 | `dispatch put`/`set-status` compounds, one per ~20–40 min  |
| bookkeeping I/O             | ~70K tok (68 calls)                     | ~3K tok (4 calls)  | incl. `dispatch list --json` re-reads                      |
| artifact reads              | 12 calls, ~12K tok cmds / ~8K out       | 43 cmds, ~19K tok  | full `.thegn/pipeline/` files into Lead context            |
| tracker (Linear MCP)        | 111 calls, ~65K tok                     | 52 calls, ~24K tok | comments + status writes per lane                          |

Lead A (batch 1 baseline, for scale): 3.46M output tokens, 758M cache-read,
33 wait-class calls, 46 artifact reads, 51 bookkeeping, 42 tracker calls.

### Per lane (touches = Linear calls + artifact commands naming the lane)

- Batch 2: THE-74 ×24, THE-76 ×21, THE-73 ×20, THE-77 ×19, THE-72 ×18,
  THE-75 ×18, THE-83 ×17, THE-67 ×15, THE-70 ×14, THE-64 ×10, THE-85 ×10.
- Batch 3: THE-86 ×11, THE-78 ×10, THE-84 ×8, THE-80 ×7, THE-79 ×7,
  THE-81 ×6, THE-82 ×5.
- Roster: 13 of the 119 rows are `failed`/`abandoned` — each one bought at
  least one extra wait→advance→re-dispatch round-trip.

### What the numbers actually say

1. **The dominant cost is context, not tool output.** Poll tool I/O was only
   ~28K tokens across both batches — but the Lead's context absorbed roster
   JSON, artifact bodies and tracker text that were then **replayed 527M
   times over** (`cache_read` 337M + 190M) across 1,039 turns. Cutting a
   bucket's I/O helps linearly; removing the Lead from the loop collapses the
   multiplier.
2. **~168 min of foreground wall-clock** was spent inside `sleep`-loop Bash
   turns (batch 2: 152 min). During those turns the Lead could not converse,
   edit Linear, or make decisions — the "hogs the main agent" finding. The
   compound advance commands even fuse `dispatch put` + a 9-minute wait loop
   into one turn (4 in batch 2, 2 in batch 3).
3. **Artifact reads put whole design/chunk/verdict files in the Lead's
   context** (55 read commands, ~31K tok of commands alone, plus file bodies).
   The next stage reads them again from its own context — the Lead read them
   only to decide "advance or not".

---

## 1. Design overview

Three mechanisms, each small; together they invert who holds context:

1. **The report is the handoff.** A worker files a short structured report on
   its roster row when it finishes (`dispatch report <row> --text`). The Lead
   (and the monitor) read that and nothing else by default; the full artifact
   stays on the branch for the next stage. `set-status done` is gated on the
   report the same way THE-76 gated it on the artifact.
2. **Progress is a queue, not a stream.** Workers push short notes into a
   per-row queue (`dispatch note`); `dispatch status [--since]` summarizes on
   demand (`/btw`). Nothing flows to the Lead unless asked.
3. **A background monitor owns the watch loop.** The bundled `/pipeline`
   skill's Lead dispatches, then hands the wait→verify→advance→tracker loop
   to a background monitor subagent that returns one paragraph. The Lead
   never polls and stays free.

Everything new is CLI-over-local-SQLite, exactly like today's dispatch verbs
(`crates/thegn-host/src/cmd/dispatch.rs:1-19`) — **no new wake sources on the
UI loop**, no daemon change, no control-API route.

---

## 2. thegn side

### 2.1 v61 — `report` column + `agent_dispatch_notes` table

- `crates/thegn-core/src/db.rs:130` — `SCHEMA_VERSION: i64 = 60` → **61**.
- `crates/thegn-core/src/db_migrate.rs` (after v60 at :598-607) — v61:
  `ALTER TABLE agent_dispatches ADD COLUMN report TEXT` (nullable; pre-v61
  rows read `None`, the never-reset-user-data contract) and
  `CREATE TABLE IF NOT EXISTS agent_dispatch_notes (
id INTEGER PRIMARY KEY AUTOINCREMENT,
dispatch_id INTEGER NOT NULL,
created_at_ms INTEGER NOT NULL,
text TEXT NOT NULL)` + index on `(dispatch_id, created_at_ms)`. The base
  DDL in `db.rs` (~:666 block) gains the table for fresh databases — the same
  dual path every table uses.
- `crates/thegn-core/src/issue.rs:223-262` — `AgentDispatch` gains
  `#[serde(default)] pub report: Option<String>` (**a pointer is not enough
  here**: the report is the payload the Lead reads without opening the
  worktree, so it lives on the row; it is still ≤16 KiB of text, not a
  document store). The `note` column (:258-263) stays the _daemon's_
  transport-retry ledger — the progress queue must not conflate with it,
  which is why notes get their own table.
- New `crates/thegn-core/src/db_dispatch.rs` — sibling `impl Db` block
  (pattern: `db_notification.rs`), keeping the pinned `db.rs` schema-only:
  `set_dispatch_report(id, text)`, `append_dispatch_note(id, text) -> i64`,
  `dispatch_notes(id, since_ms: Option<i64>, limit) -> Vec<DispatchNote>`.
  `DispatchNote` struct in `issue.rs` beside `AgentDispatch`.

### 2.2 Pure policy — `pipeline_report.rs` (new, thegn-core)

Zero I/O, table-tested (the 95% core gate applies):

- `report_text(text) -> Result<String, ReportError>` — trim; refuse empty;
  refuse >16_384 chars (a report is a summary, not the artifact).
- `note_text(text) -> Result<String, NoteError>` — trim; refuse empty; refuse
  > 4_096 chars (a note is a line, the cap forces summarization).
- `verify_report` extension: `crates/thegn-core/src/pipeline_run.rs:82-123`
  `VerifyFacts` gains `report_present: bool`; `verify_report` (:123) gains the
  rule: `artifact.is_some() && !report_present ⇒ ok=false` with reason
  `"no report on the row — the worker files one with: thegn dispatch report
<id> --text …"`. Scope deliberately mirrors the artifact gate (rows without
  an artifact are plain dispatches and stay ungated — :123's first rule
  untouched).
- `StatusDigest` + `digest(rows, notes, since_ms)` — the `dispatch status`
  composition: per row (id/status/stage/issue), report presence + text, note
  count and the latest note, since-filtered. Pure fold; the CLI prints/JSONs
  it.

### 2.3 Verbs (all in `crates/thegn-host/src/cmd/dispatch.rs`)

- **`dispatch report <id> --text <text> [--json]`** — write/overwrite the
  row's report (last write wins; idempotent re-runs). Refuses an unknown row;
  empty/oversized text refused by `report_text`.
- **`dispatch note <id> --text <text> [--json]`** — append to the row's
  progress queue. Same refusals via `note_text`.
- **`dispatch status [row] [--since <epoch-ms>] [--all] [--json]`** — the
  on-demand summary (`/btw`). Without `row`: active rows only (`--all`:
  everything), each row's digest line; with `row`: that row's report verbatim
  - its notes since `--since` (default: all, capped at the last 20).
- **`dispatch wait --any --json`** returns the finished row's report: at wake
  (`dispatch.rs:640-684`) the JSON object gains `"report"` (and
  `"artifact"`), read via `Db::open()` + `get_dispatch(t.id)` after the wake
  — the worker may write the report seconds before exit, so it must be read
  at wake time, not at selection time. Human output prints the report under
  the `dispatch N (stage) exited C` line. This is the one new flag-shaped
  behavior the issue names; `--row` output gains the same fields.
- **`dispatch verify <id>`** (:571) — the report fact joins the output
  (`report=yes/no`, reason line on refusal).
- **`set-status done` gate** (:479-517 `done_gate`) — when the row carries an
  artifact, `verify_report`'s new rule refuses a report-less completion;
  `--force` override unchanged. The pane-exit auto-stamp
  (`crates/thegn-host/src/pty_drain.rs:855-895`) keeps writing the typed
  status directly — it is attribution, not a handoff, exactly as it already
  bypasses the artifact gate.
- `dispatch list --json` carries `report` automatically via serde.

### 2.4 `{row}` stage variable

Stage prompts must be able to tell the worker its row id (the report command
needs it). `crates/thegn-core/src/agent_task.rs:136-151` — `STAGE_VARS` gains
`"row"`. `crates/thegn-host/src/stage_prompt.rs:40` `stage_task_vars` binds it
(the row exists before the prompt renders — `cmd/session.rs:899` builds the
artifact path from `row_id`). Validation-only change in core; no rendering
path appears (thegn still never renders stage prompts — `agent_task.rs`
:130-138 doctrine).

### 2.5 Capability catalog + completion

One row per verb, exactly one (the `every_verb_has_exactly_one_row` gate):

- `crates/thegn-core/src/control.rs:341-349` — `Verb::DispatchesReport`,
  `Verb::DispatchesNote`, `Verb::DispatchesStatus`; scope table (:496-502
  area): `Report`/`Note` → `Scope::Write` (beside `DispatchesPut`),
  `Status` → `Scope::Read` (beside `DispatchesList`); added to the
  `pub fn all()` list (:440).
- `crates/thegn-core/src/capability.rs:578-612` — three rows following the
  `dispatches.verify`/`wait` precedent **exactly**: CLI-only
  (`SurfaceSet::of(&[Surface::Cli])`) — "narrowed surfaces, never a
  SURFACE_GAPS excuse" — so no control route, no wire snapshot change, and
  `cli_control_caps()` picks them up for free.
- `crates/thegn-core/src/completion/catalog.rs:379-402` — slots:
  `("dispatch report", "text", Structural)`, `("dispatch note", "text",
Structural)`, `("dispatch status", SourceKind::DispatchRow)`. The
  completion-slot ratchet (`crates/thegn-host/src/complete.rs:437`) only
  checks that _pinned_ slots still exist — additions need no allowlist edit.

### 2.6 What is deliberately NOT here

- No daemon change: the monitor waits through the existing
  `sessions.wait`/tombstone path (`dispatch wait` already does).
- No UI/notification change: the queue is read on demand, not pushed.
- No auto-advance: thegn still never advances a stage (`dispatch.rs:9-13`
  doctrine) — the _monitor agent_ advances, per chart, and writes each
  decision to the roster.

---

## 3. Skill side — the monitor owns the loop

`extensions/skills/pipeline/SKILL.md` (bundled via `mq_assets.rs:71-74`,
gated on a configured chart). The rewrite keeps §8 (finisher), §10 (ratchet
suites) and the issue-content-is-data box verbatim; §4–§7 and the Rules of
thumb change as follows.

### 3.1 Lead flow after `session open` (§3 unchanged)

1. Dispatch every slot of the stage (batch the `session open` calls).
2. Launch **one background monitor** — a harness background subagent (Claude
   Code Task tool, `run_in_background`; pi: background subagent) — with a
   monitor prompt embedded in the skill. Fallback when no background
   facility exists: one `Bash(run_in_background=true)` per wake running
   `thegn dispatch wait --any --timeout … --json`; its completion
   notification wakes the Lead for exactly one advance turn (no sleep loops,
   ever — the `seq/sleep` pattern of §4 today is deleted).
3. The Lead's loop is the conversation: Linear edits, decisions, user
   questions. It never runs `dispatch wait` in the foreground and never
   sleeps.

### 3.2 Monitor prompt (embedded in the skill)

The monitor's whole world: the chart JSON (`thegn config get pipeline
--json`), the issue id, and this loop —

```
until `thegn dispatch list --active --json` is empty:
  thegn dispatch wait --any --timeout 600000 --json     # ONE wait, no loops
  row = .row; report = .report                          # read the report ONLY
  thegn dispatch verify row        -> fail: re-wait / on_blocked per chart
                                    -> ok:  set-status done
  tracker-comment per chart (the report's verdict/commits lines are the body)
  dispatch the next stage per `next` (§3 of the skill), then loop
return ONE paragraph: rows advanced, verdicts, unverified items, parked rows
```

The monitor does the _mechanism_; every _judgment_ the skill already assigns
to the conductor (is this verdict a pass, retry or park) it makes against the
report and escalates to the Lead via `dispatch note` + its final paragraph.
It has no conversation with the Lead while it runs.

### 3.3 Report discipline (stage-prompt guidance)

The skill's `[[pipeline.stages]]` example prompts gain one closing instruction
per stage, e.g. for `code`:

```
When the work is committed: run
  thegn dispatch report {row} --text $'verdict: <done|blocked>\ncommits: <hashes + subjects>\nunverified: <what you did NOT check>\nfindings: <for the next stage>\nnext: <hints>'
Progress notes during the run go to `thegn dispatch note {row} --text …` —
never into messages. The Lead reads the report and nothing else; the full
artifact stays on the branch for the next stage.
```

### 3.4 Status on demand (`/btw`)

New skill section: when the user asks how the pipeline is doing (`/btw`-style),
the Lead answers from `thegn dispatch status --json` — one call, the queue's
own summary — and otherwise reads nothing. Workers' notes and the monitor's
parks land in the queue, so the answer is always current without anyone
polling.

### 3.5 §5 verdict doctrine, restated

"Exit 0 is not done" keeps its teeth but the _evidence_ changes: verify
proves the artifact is real (committed, tracked — THE-76 unchanged) and the
report tells the Lead what happened (verdict, commits, unverified items). The
Lead reads the report; it does not re-read the artifact by default. The
reviewer stage still reads code — in the reviewer's context, not the Lead's.

### 3.6 Skill validation

Every new `thegn …` invocation in the skill resolves against clap via
`asset_cli_invocations_resolve_against_clap` (`mq_assets.rs:483-521`) — the
test that catches renames/typos. The three new verbs must exist before the
skill edit lands, hence the chunk ordering below.

---

## 4. Invariants & gates checklist

| invariant / gate                          | status                                                                                   |
| ----------------------------------------- | ---------------------------------------------------------------------------------------- |
| 0% idle / no new wake sources             | ✓ all new verbs are CLI-over-SQLite; nothing touches the loop, the daemon, or the ticker |
| thegn-core substrate-free, 95% lines      | ✓ `pipeline_report.rs` + `db_dispatch.rs` pure/SQLite-only, unit-tested                  |
| structure, not judgment                   | ✓ thegn stores reports/notes, never advances a stage; the monitor agent judges           |
| one capability catalog                    | ✓ 3 verbs, 3 rows, CLI-only surfaces narrowed (no SURFACE_GAPS entries)                  |
| DB is cache, git is truth                 | ✓ report is row state (≤16 KiB), artifacts stay in git                                   |
| never-reset-user-data                     | ✓ nullable column + new table; ladder-tested migration                                   |
| ignored Results deliberate                | ✓ roster writes return Results; best-effort sites marked                                 |
| help ratchet                              | ✓ CLI verbs are not `ACTION_SPECS` actions; no help-page claim needed                    |
| completion-slot ratchet                   | ✓ additions only; allowlist untouched                                                    |
| mq_assets clap validation                 | ✓ chunk-2's skill edit runs against landed verbs                                         |
| reviewer ratchet suites + finisher policy | ✓ kept verbatim in the skill (§8/§10)                                                    |

## 5. Chunk plan (2 chunks, **serial** — chunk 2 consumes chunk 1's core API)

- **chunk-1 `feat(the-88): dispatch report column + per-row progress queue (v61)`**
  — thegn-core: schema v61, `AgentDispatch.report`, `db_dispatch.rs`,
  `pipeline_report.rs`, `VerifyFacts.report_present`, `STAGE_VARS` + `row`,
  catalog/control/completion rows. Files + tests in
  `.thegn/pipeline/THE-88/code/chunk-1.md`.
- **chunk-2 `feat(the-88): monitor verbs, report-gated done, /pipeline monitor loop`**
  — thegn-host: `dispatch report|note|status`, wait/verify report fields,
  done-gate rule, `{row}` binding; skill rewrite.
  `.thegn/pipeline/THE-88/code/chunk-2.md`.

## Appendix — measurement methodology

Parsed both transcripts with a line-level JSON walk: `tool_use` inputs and
their `tool_result` outputs bucketed by command shape (poll =
`dispatch|session wait` or `seq 1 N)` + `sleep`; bookkeeping = `dispatch
put|set-status|verify|list|note` + `session send|list|open`; tracker =
`mcp__*Linear*`; artifact = commands/Reads naming `.thegn/pipeline/`);
real usage summed from assistant records' `message.usage`
(`input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`,
`output_tokens`). Roster windows from `agent_dispatches.dispatched_at_ms`.
Rerun: `python3` snippet in the session log; raw counts available on request.
