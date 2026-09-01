# Chunk 2 — PR/diff presentation, fetch lifecycle, and agent handoff

## Files touched

- `crates/thegn-host/src/review_rows.rs` (new): shared host projection of core
  anchored review rows onto the existing `PrDiff`/`DiffLine` row model.
- `crates/thegn-host/src/review_handoff.rs` (new): pure selection/pane-target
  policy plus off-loop live/headless handoff orchestration; no renderer code.
- `crates/thegn-host/src/pr_view.rs`: inline Files-tab thread rows, outdated
  block, resolved toggle, file counts, next/previous and jump navigation, and
  `p/P` outcomes.
- `crates/thegn-host/src/diff_view.rs`: explicit Worktree/PR-review source,
  shared interleaved rows, top-level feedback block, source/staleness footer,
  and navigation outcomes.
- `crates/thegn-host/src/actions.rs`: generation-tagged cached/live review
  fetch, cache delivery, PR/diff handoff dispatch, waker pulses, and safe
  status/error degradation; all blocking work remains off-loop.
- `crates/thegn-host/src/hydrate.rs`: load and refresh the review snapshot using
  the existing definitive-state cache rules without clobbering stale data.
- `crates/thegn-host/src/run.rs`: drain review deliveries, route modal actions,
  resolve the active worktree’s own agent pane, and focus after live paste.
- `crates/thegn-host/src/pane_writer.rs`: only if the shared helper needs a
  narrowly-scoped no-final-newline/control-sanitization seam; do not duplicate
  the existing bracketed-paste implementation.
- `crates/thegn-host/src/panel/mod.rs`: carry the cached review summary/source
  metadata needed by the panel.
- `crates/thegn-host/src/panel/sections/git.rs`: top-level feedback and
  per-file unresolved counts in the PR panel.
- `crates/thegn-host/src/panel/sections/changes.rs`: source/status affordance
  only; do not re-anchor PR lines onto the local working-tree hunk model.
- `crates/thegn-host/src/panel/gitfull.rs`: expose the PR-review source from
  the full Changes/diff surface without changing staging semantics.
- `docs/help/review-a-pr.md`: authored claims for every new view key and the
  safety/staleness behavior.
- `test/help-prose-ratchet.txt`, `test/help-context-ratchet.txt`,
  `test/help-panel-prose-ratchet.txt`, `test/help-ratchet.txt`: shrink-only
  updates only where the authored page now covers an existing allowance.
- `test/glyph-literal-ratchet.txt`, `test/color-literal-ratchet.txt`: update
  only if the shared-row extraction changes the pinned file ownership; no new
  literal allowlist debt.

## Approach

Extend the existing generation-tagged PR fetch to read cache first and fetch
`conversation + pr_diff` through `forge_handle::get().for_loc(&loc)` on a
blocking worker. Write a complete snapshot atomically, pulse the waker, and
drop stale generations. Unsupported, offline, stale, and missing-PR states
remain visible and labeled.

Use one shared row builder for PR Files and DiffView PR-review mode. In DiffView,
Worktree is the default; `Tab` changes to PR review only when a compatible
snapshot exists. `Enter` on a PR thread jumps to its exact file/line, while an
outdated/top-level item reports no anchor. The existing local staging/Changes
rows remain unchanged.

Implement live handoff by finding an actual agent foreground process inside the
active worktree group, never merely using the focused pane. Paste through the
existing nonblocking pane writer with bracketed paste and no trailing newline.
When no live pane exists, reuse the existing `PrReview` template and
`agent_run` off-loop for a confirmed headless dispatch; when no agent resolves,
surface a status and do nothing. Do not add a public control route or bypass
MCP’s `--allow-session-input` interlock.

Use existing theme slots, glyph chokepoints, and diff rendering helpers. Keep
the new modules small so `pr_view.rs`, `diff_view.rs`, `actions.rs`, and
`run.rs` do not become new god files.

## Overlap/dependency

This chunk is file-disjoint from chunk 1 but depends on its core/cache APIs;
run serially after chunk 1. All host presentation and handoff files are in this
single chunk so no coder split can create a cross-chunk overlap in `pr_view.rs`,
`actions.rs`, or `run.rs`. The help and ratchet edits are part of the same
commit because keyboard claims and their ratchets must land atomically.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host pr_view`
- `cargo nextest run -p thegn-host diff_view`
- `cargo nextest run -p thegn-host review_handoff`
- `cargo nextest run -p thegn-host help::ratchet`

Also run the repository’s scoped glyph/color/help ratchet tests if their files
are touched. Do not run e2e or re-record snapshots; do not run `just test`,
`just ci`, a full-workspace build, a migration, or the binary against the live
state DB. If a manual binary probe is unavoidable, set `XDG_STATE_HOME` to a
new temporary directory first.

## Done criteria

- PR Files and full-screen DiffView PR-review mode show exact inline anchors,
  outdated feedback, top-level comments, unresolved counts, resolved toggle,
  next/previous, and file:line jump using the shared row model.
- Local Worktree diff and staging behavior remain unchanged and are never
  falsely decorated with PR-head anchors.
- Fetch/cache writes are off-loop, generation-safe, waker-pulsed, atomic, and
  stale-preserving; unsupported forge capabilities degrade visibly.
- `p` hands one selected thread and `P` hands all unresolved threads using the
  existing `PrReview` template; live paste is nonblocking, bracketed when
  requested, sanitized, and never auto-submits. Headless fallback is confirmed,
  off-loop, and keeps the existing PR safety rules.
- Help pages claim every view-internal key; relevant help/glyph/color ratchets
  pass without adding debt. Expected e2e snapshot paths are listed in the
  architect design and are not re-recorded here.
- Commit exactly as: `feat(the-27): integrate PR comments and agent handoff`
