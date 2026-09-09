# Design — merge-queue ambient surface relocation

## Data flow (nothing new is fetched)

The token is a pure projection of state the model already carries:
`model.panel.merge_queue` (hydrated queue rows) partitioned per repo by the
same membership the panel section and repo-scoped chip use. No new wake path,
no new poller: queue-row changes already arrive over hydration and the
`DriveMsg` channel (`add-merge-queue-tui`) with a waker pulse.

- **Damage channel:** the token is chrome — a row-status change sets the
  master `dirty` (a `Full` frame), exactly like today's badge. No change to
  `render_plan::plan` or its invariant tests; pane output still never
  recomposes chrome.
- **Partitioning:** per-workspace grouping must key on the workspace's
  `repo_path`, the byte-for-byte string the sidebar keys folders by (the
  `merge_lifecycle::workspace_repo_path` lesson) — not a re-canonicalized
  path computed at draw time (no stat calls on the loop).

## The token is per-repo; the chip was per-focus

`push_mq_badge` filters the global `merge_queue` rows by
`SidebarStatus::repo_scope` — the _active_ repo — and fails open to counting
everything when scope is unknown. The header token dissolves that ambiguity:
each workspace row shows its own repo's rollup, dormant workspaces included,
so a red queue in a background repo is visible without switching to it (the
chip could never show this). The fail-open case disappears — a row that
cannot be attributed to a workspace simply contributes to no token.

## Placement and truncation on the header row

The header already composes as `Line::Split { l: label…, r: warm-pool chip }`.
The MQ token joins the right cluster, ordered before the warm chip (urgency
outranks provisioning). Right-cluster items yield to the label: on a narrow
sidebar the token drops its count first (`⚑` alone), then disappears —
mirroring the statusbar's fit priorities so nothing ever wraps a header row.
Hit-testing rides `build_sidebar`'s single layout pass (the same geometry the
renderer paints — the `RowHit` contract), so the token's click cell can never
drift from pixels; clicks outside the token cell keep today's row semantics
(activate / caret-fold).

## The `[bars]` widget

`mq` becomes an ordinary bottom-bar widget id (validated, unknown-id-skipped
like the rest), rendering exactly today's chip including its overlay
activation. Default slots omit it. `BarBadge::MergeQueue` keeps its fit
priority for users who re-enable it. The PR-queue chip is deliberately left
alone: its queue is forge-global, not repo-rooted, so the project token
argument does not transfer — symmetry is a non-goal (open question below).

## Degradation

- Colors: `Tok::Hue(Red/Amber)` + `Tok::Slot(S::Dim)` tokens only, quantized
  once in `wire.rs::color_spec`.
- Glyphs: the shared `MqStatus::glyph` vocabulary through
  `caps::active_glyphs()` (BMP width-1, ASCII fallback) — no draw-site
  literals; the color/glyph ratchets stay shrink-only.
- Rail mode: a tint on the workspace initial for red/amber only. Rationale:
  the rail's job is "is anything happening"; dim-level detail would make
  every populated queue nag from a 4-column strip.

## Help context

No new zone or panel context: the token lives in `zone:sidebar` →
`docs/help/sidebar.md`, the widget in `docs/help/bars.md`, and the behaviour
in `docs/help/merge-queue.md`. Any new bindable action (e.g. an explicit
"open this project's queue" on the header row) must be claimed by a help
page's `actions:` frontmatter and mentioned in prose (both help ratchets).

## Security

- **No new write surface, no new capability-catalog row:** the token reads
  cached DB rows already surfaced elsewhere; activation opens an existing
  overlay. No credential, sandbox, or scope-model implications.
- **Blast radius:** a wrong rollup mislabels a header — cosmetic; the queue's
  actions remain behind the section/overlay, unchanged.

## Alternatives considered

- **Keep the chip AND add the token** — rejected as the default: the issue is
  explicit ("move"), and two ambient carriers for one queue is the clutter
  being removed. The `[bars]` opt-in preserves the choice.
- **Panel-tab attention marker** (a dot on the Work tab label) — deferred;
  add-merge-queue-tui's notification kinds already route attention, and the
  token covers the collapsed-panel case (its counts don't depend on the panel
  being visible).
- **Token on the tabbar / masthead** — the masthead is stats-only and the
  tabbar is per-worktree; the queue is per-repo, which is the workspace row.

## Open questions

- Should the PR queue get the same treatment later (its chip shares the
  grammar by design)? Left to a follow-up once this pattern proves out.
- Whether the sidebar-hidden case (sidebar off entirely, rail included) needs
  a fallback nag beyond notifications — the current lean: no; notifications
  (`queue_needs_human` alert) already cover it.
