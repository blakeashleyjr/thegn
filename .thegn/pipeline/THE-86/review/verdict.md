# Security / test / bug review — THE-86 (agent pipeline v3)

Reviewer: SECURITY/TEST/BUG lane · Branch `tg/the-86-pipeline-v3` · Verdict on
HEAD `c70025c3` (three review-fix commits included). **PASS** — ready for the
merge queue (`thegn integrate` is the Lead's step, not run here).

Merge of main: already in (`9cc7b6af`; merge-base == main tip `9715b74a`, so
`git diff main...HEAD` is the full 44-file branch diff — reviewed in full, not
sampled).

## 0. Mandatory gates (isolated `XDG_STATE_HOME` per run; no migration or

binary ever touched the live state DB — the "thegn migrate" banner the binary
prints is the read-only `~/.superzej`→`~/.thegn` brand-dir _warning_ from
`migrate_brand.rs`, not a state-DB migration)

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example)
| test(control_schema) | test(capability)'` — **26/26**
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(mq_assets) | test(platform_ratchet)'` — **96/96**
- Scoped: core `pipeline_resume/pipeline_exit/pipeline_chunk` 44/44 (after
  fix), `db_migrate` incl. v58/v59/v60 shapes 12/12; host `session`/`dispatch`
  77/77, `resume_work_tests` + observer stub tests 9/9.
- `cargo clippy -p thegn-core --tests` / `-p thegn-host --tests` — clean.
- New smoke check executed standalone against the built binary in a temp
  state home: done-row resume refused, rc=1, wording verified.
- e2e: **no frame changes** — the board/sidebar diffs are `NewDispatch` test
  literals only; no view renders `note`/`chunk_path`. **No snapshots owed.**

## 1. Findings — FIXED on this branch (one commit each)

1. **`--resume-work` accepted any row, including a closed one** (addenda:
   "a row in a terminal state is refused"). `resume_row_checks` checked only
   existence + stage; a `done` row could be re-driven into a fresh finisher
   dispatch. Fix `4719f49e`: refuses `done` / `merged` / `abandoned` offline
   (pre-connect), naming the status and the rule; smoke check added.
   Interpretation recorded: `failed` — the feature's whole point per the issue
   title — and the active/parked states (`running`/`waiting_human`/`pr_open`)
   stay resumable; refusing every `is_terminal()` state would have disabled
   the verb itself. Unit tests pin both directions.
2. **Transport-retry relaunch clobbered a verdict written during backoff**
   (race). `handle_exit` parked the row `waiting_human`, slept up to 60 s of
   backoff, then relaunched and forced the row back to `running` without
   re-reading it: a `set-status done/abandoned` the Lead wrote in that window
   was overwritten and a second agent spawned over a closed stage. Fix
   `59232222`: the row is re-read after the sleep and relaunched only while
   still `waiting_human`; a taken-over row skips and drops its retry budget.
   Regression test drives the real park→sleep→re-check path against the stub
   service (claude = CONTINUE cap, 150 ms backoff, concurrent verdict task).
3. **The v60 migration test never ran** —
   `pre_v60_db_gains_the_dispatch_chunk_path_column_…` had no `#[test]`
   attribute: clippy flagged it dead and nextest silently skipped it, so the
   v60 shape was unverified in the suite despite "12/12" claims (the filter
   matched it; the runner didn't). Fix `c70025c3`; now runs and passes.

## 2. Adversarial surface — verified clean

- **Resume prompt composition (hostile text).** Issue title/body, artifact
  state, `git status --porcelain`, diff stat, and the previous screen all flow
  through `pipeline_resume::finisher_prompt`, which ANSI/OSC/CSI/control-
  sanitizes every input (newlines survive, `\r`/BEL/ESC do not; test-pinned).
  No shell construction happens in the composer; the prompt reaches the
  harness as one `sh_quote`d argument via the pre-existing `command_for` path
  (metacharacter survival is pre-pinned by `a_prompt_full_of_shell_
metacharacters_survives`). The daemon relaunch nudge is a fixed string.
- **Artifact path confinement.** `pipeline_run::artifact_path` whitelist-
  sanitizes issue/stage components to `[A-Za-z0-9._-]` (no `/`, no `..`,
  fallback on empty) — the stamped pointer is always relative and row-keyed;
  `verify_facts` reads it under the row's worktree and applies the symlink-
  strict regular-file rule. `git ls-files`/`git status` run with the worktree
  as cwd and the path as a post-`--` argument — no shell, no injection.
- **Row parenting.** The retry row gets `parent_id = Some(row.id)`, a fresh
  row-keyed artifact path (D6), inserted BEFORE the open (D5), failures after
  insert mark it `failed` — never stuck `queued`.
- **Transport classifier.** `classify` hard-refuses `failed == false` (exit-0
  is never re-read as a failure); signatures match against the CONFIGURED
  pattern list, never the screen text, so notes (`transport: <sig> (attempt
n/m)`, `limit: <sig>`, exhausted form) are injection-free and stable.
  Auth errors and unmatched failures classify to `None` = the supervisor's
  call — the daemon never retries a real failure and never writes
  `done`/`failed` (park-only, stub-test-pinned). Backoff saturates
  (`saturating_mul/pow`, 60 s cap), attempts are bounded, counters live only
  in the observer (a daemon restart legitimately resets them; the note column
  keeps the ledger).
- **CONTINUE cap.** Bit 32 ⇔ `continue_command()` pinned by
  `caps_agree_with_ops` for every registry harness; only claude/pi advertise
  it; codex/aider/antigravity relaunch cold. `continue_last` is wire-additive
  (`#[serde(default)]`, MCP surface stays `false`), and the id-free form means
  no session id is ever interpolated into a command.
- **Chunk gate.** The parser fails closed (unclosed block / unclosed inline
  list / duplicate known key ⇒ refusal naming the line; `--force` skips read
  AND parse and is reported); the overlap/after verdict is pure and
  bidirectional (`config_*.rs` ↔ `config_pipeline.rs` collide both ways);
  every conflict is reported, not the first. `session open --chunk` has no
  `--force` (clap), so the two callers cannot drift. Sibling scopes read
  best-effort from each sibling's own recorded worktree (a broken sibling
  degrades empty rather than wedging the gate). `after:` respects terminal
  statuses (`done` feeds the gate, other terminals drop out).
- **DB.** v59/v60 are `let _ =` ALTERs (idempotent across shared-file branch
  DBs) with both pre-shape tests; `dispatch_by_session` and
  `stamp_dispatch_note` are parameterized queries (no SQL injection);
  `DISPATCH_COLS`/`map_dispatch` moved together so reads can't drift.
- **Config.** `[pipeline.transport_retry]` validated (empty signature,
  `max_attempts = 0` while enabled); replace-not-extend list semantics
  documented; env-overlay/example/schema gates green.
- **Skill.** Bundled `SKILL.md` is clap-validated (`mq_assets::asset_cli_
invocations_resolve_against_clap`, green in the mandatory gate); the prose
  keeps the issue-text-is-data rule, exit-0-is-not-done, verify-then-done, and
  the finisher pattern.

## 3. Accepted deviations / residual risks (recorded, not blocking)

- **`failed` is terminal but resumable** — deliberate (see 1.1); the
  design/issue language ("resume from a failed row") and the binding addendum
  conflict only on the surface; this review resolves it as
  done/merged/abandoned refused.
- **`read_chunk_file` honors absolute `--chunk` paths** ("resolved relative …
  when relative" is the documented contract). A hostile or mistyped absolute
  path is an arbitrary-file _read_ of the invoking user's own files, parsed
  for frontmatter only — parse errors never echo file content (checked), and
  the CLI already runs at that user's full authority, so no privilege
  boundary is crossed. Hardening suggestion for a later change: confine the
  pointer under the worktree.
- **Tiny residual window** between the post-backoff re-check and the
  `stamp_dispatch_run`/`Running` stamps — a same-instant Lead write there
  could still be clobbered. The 60-second sleep was the exploitable window;
  this one is not schedulable in practice.
- **Observer lag** (`RecvError::Lagged`) skips exits — pre-existing feed
  semantics, documented in the task header; the row stays visible on the
  roster and `dispatch wait` still answers.
- **Resume of a `waiting_human` row while a retry is parked** can produce two
  agents for one stage (Lead resumes during the observer's backoff AND the
  re-check passes because the status is still `waiting_human`). Requires the
  Lead to resume a row the observer has publicly parked as retrying; the
  note column names the attempt, and the Lead owns the collision.
- **Default signature lists are generous substrings** (`timeout`, `http 500`)
  — a misclassified real failure burns bounded attempts and parks with the
  note; config replaces the lists wholesale. Degrades safely by construction.

## 4. Coder "Unverified" dispositions

- Chunk-1: full `test/smoke.sh` not run in-turn → the new checks (incl. the
  done-row refusal from this review) executed standalone here; the full suite
  remains the pre-push gate (Lead's).
- Chunk-2: live relaunch with a real harness never driven → the stub test now
  additionally covers the re-check arm; a real `claude --continue` relaunch
  still owes an operator observation in the wild.
- Chunk-3: `session open --chunk` end-to-end and two-live-agents collision
  unexecuted → gate logic table-tested + smoke-proven; spawn-path unchanged
  from `open_stage`. Acceptable.

## 5. Verdict

PASS
