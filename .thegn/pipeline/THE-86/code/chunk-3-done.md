# Chunk 3 done — Chunk file-scope gate + pipeline skill rewrite (THE-86)

Coder stage complete. This chunk was started by a previous coder whose session
ended without committing; this finisher completed the remaining surface and
committed everything. Commits on `tg/the-86-pipeline-v3`, in order (the final
code commit carries the exact required subject):

| Commit     | Subject                                                                             | By             |
| ---------- | ----------------------------------------------------------------------------------- | -------------- |
| `7d505c1a` | feat(pipeline): pure chunk file-scope parser + gate verdict (THE-86 chunk 3)        | previous coder |
| `a57242d5` | feat(pipeline): db v60 - agent_dispatches.chunk_path column (THE-86 chunk 3)        | previous coder |
| `003a2c75` | docs(pipeline): rewrite the skill onto the one-call dispatch verbs (THE-86 chunk 3) | previous coder |
| `84b6cbf2` | **feat(pipeline): chunk file-scope gate + pipeline skill rewrite (THE-86)**         | finisher       |

## What was implemented (per the chunk spec)

- **`pipeline_chunk.rs` (pure core, committed `7d505c1a`, verified here)** —
  `ChunkScope`, `parse_frontmatter` (`---` block; `- item` and inline `[a, b]`
  list styles; bare scalar; unknown keys + their items ignored; duplicate known
  key refused; unclosed block/inline-list errors name the 1-based line;
  missing/empty frontmatter = all-empty scope — the gate is opt-in),
  `glob_match` (`*` within a segment, `**` as a whole segment across `/`,
  literal otherwise; no new dependency), `paths_overlap`, `after_unmet`,
  `ActiveScope` + `ScopeVerdict` + `verdict` (blessed `overlaps:` suppresses
  only that sibling; conflict and unmet-`after` both computed so a mixed
  problem reports everything; conflict takes the refusal). 24 tests.
- **DB v60 (committed `a57242d5`, wiring finished here)** —
  `SCHEMA_VERSION` 59→60 with the idempotent `ALTER TABLE agent_dispatches ADD
COLUMN chunk_path TEXT`; `DISPATCH_COLS`/`map_dispatch`/INSERT moved
  together; `AgentDispatch.chunk_path: Option<String>` `#[serde(default)]`,
  `NewDispatch.chunk_path: Option<&'a str>` with every literal updated
  (`cmd/dispatch.rs`, `cmd/session.rs` incl. the resume path, `daemon/service.rs`,
  `db_migrate.rs` ladder test, `issue.rs`/`db_tests.rs`/`pipeline_run.rs`/
  `monitor_pipeline.rs`/`pipeline_board/{layout,view,tests}.rs`/
  `sidebar_pipeline.rs` test literals). Ladder test `pre_v60_db_gains_…`:
  a v59 DB with a live row gains the column reading NULL, the column is
  writable, nothing resets.
- **The host gate (`cmd/dispatch.rs::chunk_gate`, `pub(crate)`)** — runs BEFORE
  the insert (a refusal leaves no row behind); resolves + parses the new row's
  chunk file (unreadable ⇒ refusal naming the path and the fix; parse error ⇒
  refusal naming the line); siblings = same issue + same worktree, active,
  carrying a `chunk_path`, each scope read best-effort from the sibling's OWN
  recorded worktree (unreadable ⇒ empty scope, never an error); `done` rows
  feed the after-set, other terminals drop out. Refusal names the colliding
  paths, the sibling chunk names + row ids, and `--force`; `--force` overrides
  everything and skips the read entirely. Callers: `dispatch put --chunk
[--force]` and `session open --chunk` (dispatch form: `requires = "stage"`,
  `conflicts_with = "resume_work"`; no `--force` there — an intentional
  overlap is declared in `overlaps:`, the explicit override lives on
  `dispatch put`). `session open --resume-work` carries the failed row's
  `chunk_path` onto the retry row, so the scope picture survives the finisher.
- **Scope display** — `dispatch list` gains a trailing `chunk` column
  (basename, `-` when unset); JSON rows carry `chunk_path` and — when the file
  is readable at list time — the parsed `chunk_files` (omitted, never empty,
  when the file is gone). `dispatch put --json` reports `"forced": true` on a
  forced dispatch (the `set-status done --force` idiom).
- **`config/config.toml.example`** — the commented architect prompt now
  requests the `files:`/`overlaps:`/`after:` frontmatter block per chunk file;
  the code-stage prompt notes the gate. Commented + validating
  (`example_config_validates_clean`, `real_example_config_generates_cleanly`,
  `example_config_prose_names_every_kind` all green).
- **Skill rewrite (committed `003a2c75`, one scan fix here)** — teaches the
  loop on the current verbs: `config get pipeline --json` + `config validate`,
  resume-before-you-dispatch via `dispatch list --active --json`, the one-call
  `session open --stage <stage> --issue <id> --adopt --json` with `--chunk`
  for coder chunks, `dispatch wait --timeout`, `dispatch verify` with the
  **exit-0-is-not-done** rule, `session close`, `session list --live`, the
  finisher pattern (`--resume-work`, automatic transport retries surfaced
  never silently re-driven), and the cheap ratchet suites. The fix in
  `84b6cbf2`: the architect-prompt sample inside the toml fence said "thegn
  refuses…" and the `mq_assets` clap scanner reads every code region as
  invocations — reworded so the bundled skill claims only real commands.
- **`docs/cli.md`** — `dispatch put --chunk` / `session open --chunk` scope
  gate + the `chunk`/`chunk_path`/`chunk_files` display documented.
- **`test/smoke.sh`** — daemon-free chunk-gate block after the THE-76/THE-86
  blocks: a second worktree of the smoke repo holds three chunk files; row A
  dispatches active, row B is refused naming the collision/row/`--force`,
  `--force` passes and says so, `after: [chunk-1]` is refused while chunk-1 is
  queued and passes once chunk-1 is `done`; `dispatch list --json` shows
  `chunk_path` + `chunk_files`.
- **Completion catalog** — `dispatch put chunk` / `session open chunk`
  classified `Structural` (filesystem path, same shape as `parent_artifact`);
  `session open resume_work` classified `Reserved(DispatchRow)` (same as
  `dispatch verify id` / `session open parent`). This also repairs a red
  `completion_slots_are_bound_or_pinned` that chunk 1's commit left behind
  (verified failing at HEAD `003a2c75` before the fix).

## Verification (scoped, per dev-loop policy — no full-workspace gates)

- `just quick thegn-core` and `just quick thegn-host` — clean; full `just
quick` (workspace) clean after the catalog change; re-clean after the
  pre-commit treefmt reformat.
- `cargo nextest run -p thegn-core pipeline_chunk` — 24/24 (parser both list
  styles + unknown keys + line-numbered errors + missing-frontmatter opt-out;
  glob `*`/`**`/exact; `paths_overlap`; `after_unmet`; `verdict` incl.
  blessing suppression, done-set, mixed conflicts).
- `cargo nextest run -p thegn-core -E 'test(ladder) or test(pre_v60)'` — 24/24
  (the chunk's literal filter `db_tests::migration` matches no module — same
  note as chunk 2; the ladder lives in `db_migrate.rs::tests` +
  `db_tests.rs::ladder_vN_*`). Includes the new v60 ladder test.
- `cargo nextest run -p thegn-core completion` — 42/42 (catalog coverage,
  kind walk) after the three new slots.
- Example config: `example_config_validates_clean`,
  `real_example_config_generates_cleanly`, `example_config_prose_names_every_kind`
  — 3/3 (this is the "`thegn config validate` accepts the example" criterion,
  exercised hermetically by the test rather than against a live state dir).
- `cargo nextest run -p thegn-host -E 'test(dispatch) or test(mq_assets) or
test(catalog_tests) or test(chunk) or test(complete) or test(help) or
test(platform_ratchet) or test(config_example)'` — 134/134, including:
  the new chunk-gate suite (overlap refusal naming paths + sibling chunk +
  row id + `--force`, and no row left behind; different worktree/issue not
  siblings; `after:` refused naming the row + status and passing once done;
  unreadable sibling degrades to empty scope; `--force` skips the read —
  proven with a nonexistent file; missing/unparseable new-file refusals naming
  the path and line 2); the `chunk_path` put→list round-trip; `chunk_cell`;
  `mq_assets` frontmatter + the clap-invocation scan (green after the skill
  reword); catalog drift; completion slots; help ratchet.
- `cargo nextest run -p thegn-host -E 'test(session)'` — 103/103 (open_stage
  preflight/vars, resume row checks, catalog surface).
- `just smoke` — **all checks passed**, including the six new chunk-gate
  checks (verified individually in the output); PTY smoke green.

## Unverified

- **`session open --chunk` end-to-end through `open_stage`** (gate → row with
  `chunk_path` → session spawn): needs a live daemon + harness. Covered
  piecewise instead: the shared `chunk_gate` is unit-tested on both refusal
  axes, the DB round-trip through the same `put` write path is pinned, and the
  clap wiring (`requires`/`conflicts_with`) compiles into the tree the
  completion/help tests walk. The resume row carrying the failed row's
  `chunk_path` is code-reviewed, not executed.
- **A real colliding dispatch between two live agents** — the gate logic is
  table-tested and smoke-proven against the roster/files, but no run drove two
  actual agent sessions into one file (by design: the gate runs before any
  spawn).
- **`just test` / `just lint` / `just ci` / coverage / e2e** — deliberately not
  run (Lead addendum: no >10-minute builds, no e2e; the pre-push gate owns
  them).
- **macOS smoke** — the smoke block uses `git worktree add` + plain files only,
  so it should be platform-neutral, but it was executed on Linux only.
