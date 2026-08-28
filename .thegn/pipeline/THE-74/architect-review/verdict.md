# THE-74 — architect review verdict

Reviewed 2026-08-27 by the architect, on `tg/the-74-pipeline-board-v2` after
`git merge main` (THE-70 sidebar/doctor, THE-83 agents/model/env, bundled
skills, `tg/stage-harness-override`). Full branch diff `main...HEAD` reviewed
against `.thegn/pipeline/THE-74/architect/design.md`, the three chunk specs +
done reports (every "Unverified" section read), and repo standards.

## Verdict

APPROVED

Small corrections were applied by the reviewer (commits below); no revision
chunk is needed.

## Merge (Lead addendum item 1)

The in-progress `git merge main` had two conflicts; resolved and committed as
`879d6f89`:

- `sidebar_view.rs` — both sides were additive test blocks (this lane's
  pipeline-row render tests vs THE-70's quick-jump digit tests); kept both.
- `docs/help/system-monitor.md` — this lane had replaced the monitor's
  Pipeline section with a pointer to the new surface (per D1) while THE-83 had
  added the bundled-`/pipeline`+`/supervise` skills paragraph. Kept the
  pointer and moved THE-83's paragraph into `docs/help/pipeline-board.md`,
  where that content now lives.

`SCHEMA_VERSION` reconciliation: main is at **57**, this branch at **58** —
next-free, no collision. THE-75 (unlanded) keeps its merge surface small: the
monitor-tab deletion is its own commit (`499c1a4a`, pure removals in
`monitor.rs` / `monitor/build.rs` / `monitor_tests.rs` / `run.rs`, exactly the
design §4 mitigation), the board lives in `pipeline_board/*`, and
`AgentDispatchStatus::glyph_set` was added beside, not over, `glyph()`.

## Directive #3 — sidebar hierarchy (the main finding)

Chunk 3's fold violated the directive on **both** named failure axes:

1. It emitted flat lane folders at the sidebar tail (door row → lane → agent →
   worktree), not `[Workspace] → "Pipelines" → [pipeline] → [worktrees]`.
2. It grouped only `status.is_active()` rows, so every folder vanished the
   moment a lane's last dispatch went terminal — and after a restart the
   folders only existed while agents were live.

Per the addendum this was fixed in-review rather than sent back
(`c3eccb97`, pure fold + tests + docs):

- `sidebar_pipeline::lanes(&[AgentDispatch])` now folds rows of **any**
  status; a lane is named from the roster's `issue_id` (basename fallback for
  issue-less rows) and carries every worktree its rows reference, deduped by
  path, earliest-reference order. 12 in-file tests, including
  terminal-row survival and fold determinism.
- Emission moved inside the workspace loop: one `Pipelines` group per
  workspace (new `RowKind::PipelineGroup`), one `PipelineLane` folder per
  issue, `PipelineWorktree` mirror leaves carrying the primary row's
  `tab_target`, resolved from the same live-session + `DbWorktree` sources
  the primary rows use. A worktree no roster row references stays where it
  was; a lane with no resolvable worktree falls back to a tail group under
  the board door (all lanes do in the flat layout). Folders are visible by
  default; collapse keys `pipeline/group:{slug}` / `pipeline/lane:{key}`.
- The agent-level row (`PipelineAgent` + `PipelineAgentRow`) is gone from the
  sidebar — the mandated hierarchy is group → lane → worktrees, and the
  volatile relative-age text (an e2e-flap risk) leaves the sidebar with it.
  Agent status remains the board's job.
- `docs/help/sidebar.md` rewritten for the new shape.
- Side cleanup: `collect_attention` lost the `stage_order` parameter chunk 3
  had threaded through `hydrate.rs` (the fold no longer orders agents), so
  that deviation is reverted too.

## Other "Unverified" items — verified or fixed

- **Chunk 1, flagged second read seam** (`dispatch_dispatched_at_ms`, the
  scalar resurrection read that bypasses `map_dispatch`): FIXED in `c3eccb97`
  — wrapped in the same `normalize_dispatch_ms` guard, with a raw-SQL
  seconds-stamp test. Every read of the column now normalizes.
- **Chunk 1**: v58 migration sits with its siblings above the version stamp,
  gated on the pre-bump version, `// best-effort:` comment present; the v57
  fixture was re-pinned to a literal so the bump didn't silently untest it.
  Verified in code and by the db_tests run.
- **Chunk 2**: `stage_sequence` shared-precedence helper verified (board calls
  with `keep_empty = true`, `ordered_rows` byte-compatible). The
  "ordered_rows now reads live config stage names" deviation is **accepted**:
  it is documented, makes the board and the row grouping unable to disagree,
  and is one line to revert. `MonitorTab::tab()` dead-code allowance predates
  the lane — left as the done-report did.
- **Chunk 2 invariants**: `pipeline_board/layout.rs` is pure (no clock, no
  model, no termwiz); the board opens through `layer::open_layer` so the
  render decision stays a derived `Full`; the sampler rides
  `board.wants_dispatches()` — no new wake source. Glyph/color ratchets pass
  with no allowlist additions.
- **Chunk 3 mouse/menu**: drag sources are kind-gated (Worktree/Workspace/
  Folder), mirrors carry empty `pin_key` so pins/marks/bulk actions can't
  double-count — the design's identity-anchor reasoning held.

## Checks actually run (scoped, per addendum)

- `just quick thegn-core`, `just quick thegn-host` — clean.
- `cargo nextest run -p thegn-host` filtered
  `pipeline_board|sidebar_pipeline|sidebar|monitor|help` — **416 passed**.
- `cargo nextest run -p thegn-core` filtered `db|issue` — **518 passed**.
- ratchet tests (glyph/color/ignored-result/etc.) — 12 passed.

No `just test` / `just ci` / `just coverage` / e2e, per the addendum.

## Notes (non-blocking)

- **e2e**: this lane changes frames (sidebar tree, monitor loses a tab). The
  committed baselines are stale repo-wide (pre-existing debt, CLAUDE.md); when
  e2e is re-enabled, re-record on both platforms. The sidebar no longer paints
  any clock-dependent text, but the **board** still renders relative ages —
  if a baseline exercises the board, pin the age in `e2e_freeze` first.
- The any-status fold means a lane persists until its roster rows are removed.
  That is the directive's explicit intent (survives a restart); pruning is a
  roster-ledger decision (`thegn dispatch del`), never a sidebar concern.
- Coverage (thegn-core 95%) not measured per the addendum; the core delta this
  review added is one wrapped call plus tests.

## Commits from this review

| commit     | subject                                                                                             |
| ---------- | --------------------------------------------------------------------------------------------------- |
| `879d6f89` | Merge branch 'main' into tg/the-74-pipeline-board-v2 (conflict resolutions)                         |
| `c3eccb97` | fix(sidebar): nest pipeline lanes under per-workspace Pipelines folders (THE-74) (architect review) |
| `20e2b106` | docs(the-74): changelog entry for the board surface + sidebar pipeline folders (architect review)   |
