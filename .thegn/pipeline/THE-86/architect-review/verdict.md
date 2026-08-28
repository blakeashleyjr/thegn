# Architect review — THE-86 (agent pipeline v3)

Reviewer: ARCHITECT lane · Branch `tg/the-86-pipeline-v3` · Verdict **APPROVED**

## 0. Lead addenda executed

- **Merge of main: done, clean** — `9cc7b6af Merge branch 'main' into
tg/the-86-pipeline-v3`. Main had moved two commits past the branch base
  `d361b60a` (`c643a4f9` lint chore + brand-guard exemption for
  `.thegn/pipeline/**`, `9715b74a` merge); THE-70/THE-83 were already in the
  base. No conflicts; pre-commit hooks (treefmt/shellcheck/yamllint) passed.
  Full-branch diff `git diff main...HEAD` reviewed: 43 files, +5121/−244.
- **Mandatory gates: green** (isolated `XDG_STATE_HOME` everywhere; no
  migration or binary ever touched the live state DB):
  - `cargo nextest run -p thegn-core -E 'test(env_overlay) |
test(config_example) | test(control_schema) | test(capability)'` — **26/26**
  - `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(mq_assets) | test(platform_ratchet)'` — **96/96**
- Lane docs: design.md + all three chunk files + all three done files read;
  every "Unverified" section dispositioned in §4 below.

## 1. Design conformance (verified in code, not in prose)

**§1 Finisher/resume.** `pipeline_resume.rs` is pure per the doctrine header
and table-tested (11 tests): the three artifact-state sentences are verbatim,
status/diff render only when non-empty, the tail truncates to 8 non-blank
lines, empty-everything still renders, determinism pinned, and — beyond the
design — embedded text is ANSI/control-sanitized so a hostile tracker body
cannot smuggle terminal sequences into the next worker's context. Good
addition; keep it. `resume_work` follows the §1.2 flow step-for-step: offline
row checks before any connect (row miss wording = `set-status`'s, non-pipeline
row refused), `stage_or_bail`, re-render against the ROW's bindings
(`{artifact}` = row's artifact, `{parent_artifact}` from the parent row),
`verify_facts` + `git status --porcelain` + `git diff --stat`, screen tail via
the tombstone-aware snapshot with degrade-to-empty on ANY failure, row-before
open (D5), artifact keyed to the NEW row id (D6), failure-after-insert marks
the row `failed`, JSON `{row, session, stage, artifact, issue, worktree,
resumed_from}`.

**§2 Transport retry.** `pipeline_exit.rs` classify/decide/notes match the
decision table exactly (backoff `base·2^(n−1)` capped at 60 s, Limit parks at
any attempt, Exhausted note pinned, exit-0 gate hard). One deliberate,
documented refinement of my signature sketch: the default transport list
matches `http 5xx-by-name` rather than bare `503`/`529` numerals — "a screen
full of token counts must not read as an outage". Correct call; the design's
intent (generous substrings, no false transport positives) is better served.
`HarnessCaps::CONTINUE` (bit 32) with bit⇔op pinned by the extended
`caps_agree_with_ops`; claude/pi carry id-free continue forms, codex/aider/
antigravity advertise none (relaunch cold). Config surface: replace-semantics
lists + validation (empty signature, `max_attempts=0` while enabled) + a
fully documented commented example block. DB v59 `note` column with
`DISPATCH_COLS`/`map_dispatch`/ladder test moved together; `stamp_dispatch_note`

- `dispatch_by_session` (newest stamp wins). `stage_prompt.rs` is the one
  seamer for CLI + daemon renders. The observer (`daemon/pipeline_retry.rs`) is
  event-driven with zero idle timers, gates on nonzero exit + `tomb.attached ==
0` + non-terminal row, parks ONLY (`waiting_human` + note — never
  `done`/`failed`, pinned by the service stub test), relaunches through
  `svc.open` (the full sandbox/cap/seeder path), re-stamps the SAME row, and
  appends `relaunch failed: …` on the failure arm. Spawned in `daemon/mod.rs`
  beside the other loops. Tombstone `attached` is the real subscriber count at
  death (`build_tombstone` reads `live.attached`; burial-before-exit is pinned
  by an existing test at `daemon/session.rs:1613`).

**§3 Chunk file-scope.** `pipeline_chunk.rs` parser: both list styles, bare
scalars, unknown keys (and their items) ignored, duplicate known key refused,
unclosed block/inline-list errors name the 1-based line, no-frontmatter ⇒
opted-out scope. Glob semantics exactly as specced (`*` within a segment,
`**` across, literal otherwise) with no new dependency. The host `chunk_gate`
runs BEFORE the insert (a refusal leaves no row behind), siblings = same
issue + same worktree + chunk_path, `done` feeds the after-set, other
terminals drop out, sibling scopes read best-effort from each sibling's own
recorded worktree, both axes computed so a mixed refusal names everything,
`--force` overrides AND skips the read. Both callers (`dispatch put`,
`session open --stage --issue`) share the one gate; `session open --chunk`
intentionally has no `--force` (declared `overlaps:` or `dispatch put`).
Scope display: `chunk` column (basename, `-` unset), JSON `chunk_path` +
`chunk_files` (omitted-when-gone, never empty — the opt-out would be a
different claim). `--resume-work` carries the failed row's `chunk_path` onto
the retry row. DB v60 `chunk_path` + every `NewDispatch` literal updated;
three completion slots added, no new verbs ⇒ catalog clean.

**§4 Skill.** Full rewrite documents every mandated verb and rule
(`--stage --issue`, `--chunk`, `dispatch wait --timeout`, `verify`,
exit-0-is-not-done, `session close`/`list --live`, the finisher pattern,
surface-don't-re-drive automatic retries, the ratchet suites, generic-roles
config). `mq_assets` frontmatter + clap-invocation scan green.

## 2. Fix-or-flag: corrections applied (commit `88429345`)

1. **`pipeline_resume.rs` — the screen-tail header lied when the tail was
   short.** It printed "(last 8 non-blank lines)" with the constant, even
   when fewer lines survived; now counts `tail.len()` (the lines actually
   quoted). Pinned by an added assertion in the blank-dropping test.
2. **`cmd/session.rs::resume_work` re-implemented the render refusal
   inline** (render_prompt + hand-rolled invalid-template wrap + empty-prompt
   bail) instead of calling the shared `stage_prompt::render_stage` — the
   exact "two implementations of one refusal would drift" pattern the design
   warns about, and which chunk 2's done file explicitly flagged for this
   review. Folded onto the helper; messages byte-identical, behavior
   unchanged, now one helper with three callers.

## 3. Deviations reviewed and ACCEPTED (recorded, no action)

- **`--agent` conflicts with `--resume-work`** (row-is-the-record) vs the
  design §1.2 prose "`--agent` wins". The chunk spec pinned the conflict list;
  the coder documented the reasoning (clap must let `--resume-work 999999`
  reach its offline refusal). The row is the record is the more coherent
  stance — a resume reproduces the stage/harness pair that failed; harness
  changes belong to config. Design §1.2 prose is superseded by this record.
- `clap`'s requirement relaxation (`required_unless_present` forms) needed to
  make the offline refusals reachable at all — verified live by the coder,
  regression-checked against the plain paths; `session` suite green.
- **A transient `dispatch wait` does NOT suppress the observer**: `wait`
  subscribes to the global event feed, not the session's `subs` registry, so
  `tomb.attached` stays 0 for a waiting Lead — the daemon retries while the
  Lead's wait returns the exit and the note lands on the roster. Only real
  pane/human attaches (THE-85 adopt, `session attach`) own the verdict. This
  is the correct reading of §2.1's scope rule; noted so nobody "fixes" it
  into a race with the Lead.
- Retry attempts live in the observer task's memory keyed by ROW id; a daemon
  restart legitimately kills the retry budget (it killed the sessions too) —
  design-sanctioned, and the note column preserves the ledger.

## 4. "Unverified" items from the done files — disposition

| Item (chunk)                                                    | Disposition                                                                                                                                                                                                                                                                                                                                                                                                          |
| --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Full `bash test/smoke.sh` not run by chunk 1 (C1)               | Moot: chunk 3 ran `just smoke` **all-green** at `95f7d5fd`, which contains chunk 1's tree plus 2/3. My post-review changes are prompt wording + an error-path-identical helper swap — nothing smoke-reachable.                                                                                                                                                                                                       |
| Live relaunch with a real harness (`claude/pi --continue`) (C2) | Accepted per the design's own test strategy: classifier + decision are core-table-tested, the service stub test drives the REAL `handle_exit` path (tombstone → row → classify → park/append → relaunch entry) with no PTY. Residual risk: the relaunch-success arm (`stamp_dispatch_run` + `Running`) is code-reviewed only; its failure mode degrades to today's behavior (Lead sees the row via `dispatch wait`). |
| `just quick thegn-host` at exact commit `c545afef` (C2)         | Moot: the final HEAD compiles clean (`just quick` core + host this review) and all scoped suites pass.                                                                                                                                                                                                                                                                                                               |
| `session open --chunk` end-to-end through a live daemon (C3)    | Accepted: needs a live daemon + harness by construction; the shared gate is unit-tested on both refusal axes, the DB round-trip is pinned through the same `put` write path, and the clap wiring is walked by the completion/help tests.                                                                                                                                                                             |
| Two live agents colliding on one file (C3)                      | Accepted — the gate runs before any spawn; the collision is decided by roster + files, both of which are under test.                                                                                                                                                                                                                                                                                                 |
| macOS smoke neutrality (C3)                                     | The block uses `git worktree add` + plain files only — platform-neutral by inspection. Noted, no action.                                                                                                                                                                                                                                                                                                             |

## 5. Gate summary (this review, isolated `XDG_STATE_HOME`)

| Gate                                                                          | Result     |
| ----------------------------------------------------------------------------- | ---------- |
| thegn-core env_overlay/config_example/control_schema/capability (MANDATORY)   | 26/26 ✅   |
| thegn-host complete/help/catalog_tests/mq_assets/platform_ratchet (MANDATORY) | 96/96 ✅   |
| thegn-core pipeline_resume/pipeline_exit/pipeline_chunk/migration             | 50/50 ✅   |
| thegn-host session + dispatch (incl. chunk-gate suite)                        | 137/137 ✅ |
| thegn-svc control (wire schema incl. `continue_last`)                         | 32/32 ✅   |
| thegn-core completion (3 new slots)                                           | 42/42 ✅   |
| `just quick thegn-core` / `just quick thegn-host`                             | clean ✅   |
| `just smoke` (chunk 3, at `95f7d5fd`, all sections)                           | green ✅   |

No UI/frame changes (`pipeline_board`/`sidebar` diffs are `NewDispatch` test
literals), so no e2e re-record is owed. Pre-push (`clippy` + `just test` +
`just smoke`) remains the Lead's final gate before land.

## 6. Verdict

**APPROVED.** All three chunks implement the design faithfully; the two small
drifts found are fixed in `88429345`; the accepted deviations are recorded
above with reasons. No revision chunks.

- Merge commit: `9cc7b6af`
- Architect fix commit: `88429345`
