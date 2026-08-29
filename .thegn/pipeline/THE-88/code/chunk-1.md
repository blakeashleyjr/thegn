---
files:
  - crates/thegn-core/src/db.rs
  - crates/thegn-core/src/db_migrate.rs
  - crates/thegn-core/src/db_dispatch.rs
  - crates/thegn-core/src/issue.rs
  - crates/thegn-core/src/lib.rs
  - crates/thegn-core/src/pipeline_report.rs
  - crates/thegn-core/src/pipeline_run.rs
  - crates/thegn-core/src/agent_task.rs
  - crates/thegn-core/src/capability.rs
  - crates/thegn-core/src/control.rs
  - crates/thegn-core/src/completion/catalog.rs
overlaps: []
after: []
---

# THE-88 chunk-1 — thegn-core: report column, progress queue, catalog rows

You are the CODER for this chunk. Work in
`/home/blake/.superzej/worktrees/thegn/tg-the-88-pipeline-token-efficiency` on
branch `tg/the-88-pipeline-token-efficiency`. The design is
`.thegn/pipeline/THE-88/architect/design.md` (§2 is yours). Read it first;
every fact below cites file:line so you can verify rather than trust.

## Goal

THE-88's thegn-core half: a worker's structured report stored on its roster
row, a per-row progress-note queue, the report-presence fact in the
run-completion verdict, the `{row}` stage variable, and the capability/
completion rows three new dispatch verbs need. **No CLI here** — chunk-2
consumes this API (serial: chunk-2 is gated on this chunk landing first).

## Files (all of them; touch nothing else)

1. `crates/thegn-core/src/db.rs`
   - :130 `SCHEMA_VERSION: i64 = 60` → `61`.
   - Schema doc list (the vNN comments at the top, :29-): add the v61 entry.
   - Base DDL (the `CREATE TABLE IF NOT EXISTS` block around :666 where
     `agent_dispatches` is created): add `report TEXT` to the
     `agent_dispatches` column list and the new `agent_dispatch_notes`
     table + `CREATE INDEX IF NOT EXISTS … (dispatch_id, created_at_ms)`.
2. `crates/thegn-core/src/db_migrate.rs`
   - After v60 (:598-607): v61 = `ALTER TABLE agent_dispatches ADD COLUMN
report TEXT` (guard with the same `has_column`-style idempotence the
     neighboring ALTERs use) + `CREATE TABLE IF NOT EXISTS
agent_dispatch_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, dispatch_id
INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, text TEXT NOT NULL)` - the index. Comment explains WHY a separate table: the `note` column
     (issue.rs:258-263) is the daemon transport-retry observer's ledger; the
     progress queue must not conflate with it (design §2.1).
   - Tests module: a ladder test following
     `pre_v60_db_gains_the_dispatch_chunk_path_column_without_resetting_anything`
     (:854): build a v60 DB with a real roster row, open with the new
     `SCHEMA_VERSION`, assert the row still reads and gains `report = None`,
     and that notes append/list.
3. `crates/thegn-core/src/issue.rs`
   - `AgentDispatch` (:223-262): `#[serde(default)] pub report:
Option<String>` with a doc comment: the worker's structured handoff
     summary (verdict/commits/unverified/findings/next), ≤16 KiB, stored on
     the row because the Lead reads it WITHOUT opening the worktree — the
     artifact pointer (artifact_path) still points at the full document,
     which stays git's.
   - New `DispatchNote { pub id: i64, pub dispatch_id: i64, pub
created_at_ms: i64, pub text: String }` beside it.
4. `crates/thegn-core/src/db_dispatch.rs` (NEW)
   - Sibling `impl Db` block (pattern: `db_notification.rs`, which keeps the
     pinned `db.rs` schema-only): - `pub fn set_dispatch_report(&self, id: i64, text: &str) -> Result<()>`
     — UPDATE; must error when the row does not exist (check
     `get_dispatch` first, naming the id). - `pub fn append_dispatch_note(&self, id: i64, text: &str) ->
Result<i64>` — INSERT with `util::now()`; row-existence check same as
     above. - `pub fn dispatch_notes(&self, id: i64, since_ms: Option<i64>, limit:
usize) -> Result<Vec<DispatchNote>>` — newest last; `since_ms`
     filters `created_at_ms > since`; `limit` caps (caller passes 0 for
     "no cap" or a count).
   - Unit tests against a `tempfile::TempDir` + `Db::open_at` (same shape as
     the `dispatch.rs` / `db_migrate.rs` test helpers).
5. `crates/thegn-core/src/pipeline_report.rs` (NEW) — pure policy, no I/O, no
   substrates (the crate-boundary gate
   `crates/thegn-core/tests/crate_boundaries.rs` and the 95% coverage gate
   both apply — test every function):
   - `pub fn report_text(text: &str) -> Result<String, ReportError>` — trim;
     error on empty; error over 16_384 chars (name the cap in the message).
     `ReportError` implements `Display`; keep it a small enum (Empty, TooLong
     { len }) — no anyhow in core policy.
   - `pub fn note_text(text: &str) -> Result<String, NoteError>` — trim;
     error on empty; error over 4_096 chars. Same enum shape.
   - `pub struct StatusDigest { pub id: i64, pub status: String, pub stage:
Option<String>, pub issue_id: String, pub report: Option<String>, pub
note_count: usize, pub latest_note: Option<(i64, String)> }` and
     `pub fn digest(rows: &[AgentDispatch], notes: &HashMap<i64,
Vec<DispatchNote>>, since_ms: Option<i64>) -> Vec<StatusDigest>` — one
     digest per row (roster order), note_count/latest computed over the
     since-filtered notes, report passed through. Pure fold; the CLI in
     chunk-2 prints or JSONs it.
   - Table tests: trim/empty/oversize for both text fns; digest with empty
     notes, notes before/after `since`, multiple rows, unknown row ids in
     the notes map (ignore them — a note for a pruned row must not panic).
6. `crates/thegn-core/src/pipeline_run.rs`
   - `VerifyFacts` (:82) gains `pub report_present: bool`; the constructor
     sites in this file's tests updated.
   - `verify_report` (:123) gains the rule (design §2.2):
     `artifact.is_some() && !report_present` ⇒ `ok = false` with reason
     `"no report on the row — the worker files one with: thegn dispatch
report <id> --text …"`. Order: artifact rules first, then the report
     rule, so a missing artifact AND missing report both appear in
     `reasons`. The `artifact == None ⇒ ok` fast path stays FIRST and
     untouched (plain dispatches never gated).
   - Tests: report rule on/off, both-reasons case, artifact-less rows
     unaffected, serialization still flat.
7. `crates/thegn-core/src/agent_task.rs`
   - `STAGE_VARS` (:136-151) gains `"row"` — with a comment: the worker's own
     roster row id, so a stage prompt can tell the worker to file
     `thegn dispatch report {row} --text …`; thegn renders nothing (the
     validate-only doctrine comment above the const explains the seam).
   - The var-set test at :1262-1271 (`stage_vars_extend_the_issue_set_and_gate_typos`)
     asserts the new name.
8. `crates/thegn-core/src/capability.rs`
   - Three rows after `dispatches.wait` (:609-614), same CLI-only shape
     (`SurfaceSet::of(&[Surface::Cli])`) — narrowed surfaces, NEVER a
     SURFACE_GAPS excuse (comment precedent at :596-602):
     - `"dispatches.report"` — "Record a worker's structured report on a
       roster row"
     - `"dispatches.note"` — "Append a progress note to a roster row's queue"
     - `"dispatches.status"` — "Summarize a roster row's report and progress
       notes"
   - `every_verb_has_exactly_one_row` and the per-surface coverage tests
     must stay green — they fail the build if a verb lacks its row.
9. `crates/thegn-core/src/control.rs`
   - `Verb` enum (:341-349 area): `DispatchesReport`, `DispatchesNote`,
     `DispatchesStatus` with one-line docs ("Observes only" for Status;
     "writes the row" for Report/Note).
   - `Verb::all()` (:440-470): add all three.
   - `required_scope` (:496-502 area): `DispatchesStatus` → `Scope::Read`
     beside `DispatchesList`; `DispatchesReport`/`DispatchesNote` →
     `Scope::Write` beside `DispatchesPut` (find that arm).
10. `crates/thegn-core/src/completion/catalog.rs`

- Slots near the dispatch ones (:379-402):
  `slot("dispatch report", "text", SourceKind::Structural)`,
  `slot("dispatch note", "text", SourceKind::Structural)`,
  and a positional for `dispatch status` following however
  `("dispatch verify", …)` (:379) is expressed — mirror it exactly.
- `test/completion-slot-ratchet.txt` must NOT change (the ratchet only
  checks pinned slots still exist; additions are free).

11. `crates/thegn-core/src/lib.rs` — `mod db_dispatch;` + `mod
pipeline_report;` in the module list, alphabetical.

## Approach notes

- Migrations run once per version gap (`db.rs` `open_mode`/ladder); the ALTER
  must be idempotent-guarded exactly like the v56/v59/v60 ALTERs above it.
- `report` is row state, not a document store: the 16 KiB cap is the line.
  Anything bigger belongs in the artifact.
- `Digest.report` passes the raw text; truncation is a presentation concern
  for chunk-2's CLI, not policy.
- No tokio, no termwiz, no git subprocess in anything new. `pipeline_report`
  is pure; `db_dispatch` is rusqlite only.

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-core
cargo nextest run -p thegn-core pipeline_report
cargo nextest run -p thegn-core pipeline_run
cargo nextest run -p thegn-core db_migrate
cargo nextest run -p thegn-core capability
cargo nextest run -p thegn-core stage_vars agent_task
cargo nextest run -p thegn-core dispatch
```

(`dispatch` matches the roster/notes tests across db modules; adjust filters
if your test names differ, but keep them `-p thegn-core`-scoped. Pre-push
runs `just test` for the whole workspace — do not run it yourself.)

## Done-criteria

1. `just quick thegn-core` clean (clippy -D warnings on lib).
2. All the scoped nextest filters above green.
3. `SCHEMA_VERSION == 61`; a v60 database migrates without losing roster
   rows (ladder test proves it).
4. `verify_report` refuses `artifact set + report missing`, names the
   `dispatch report` command in the reason, and still passes plain rows.
5. The three capability rows exist, CLI-only, scoped Write/Write/Read, and
   the catalog coverage tests pass with zero SURFACE_GAPS additions.
6. `{row}` validates in a stage prompt (`config validate` path) — the
   agent_task test proves the var set.
7. Commit with EXACTLY this subject (single commit):

   `feat(the-88): dispatch report column + per-row progress queue (v61)`

## Report (when done)

Commit your work, then file the report the Lead will read — and nothing
else:

```sh
thegn dispatch report <your-row-id> --text $'verdict: <done|blocked>\ncommits: <hashes + subjects>\nunverified: <what you did NOT check, e.g. the full just test>\nfindings: <for chunk-2 / reviewer>\nnext: <hints>'
```

Keep it under ~20 lines. The full artifact stays on the branch for the next
stage.
