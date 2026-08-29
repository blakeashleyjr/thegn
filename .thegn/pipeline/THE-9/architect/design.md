# THE-9 architect design

## Decision

Move merge-queue visibility out of the default bottom-bar badge and into two
quiet, scoped surfaces:

1. the existing **Work → Merge queue** section in the right panel; and
2. a one-line token on each full-mode workspace header, immediately before the
   existing warm-pool token.

The token is a derived view of that workspace's rendered worktree queue rows.
It shows the highest active queue tier and the number of entries in that tier:
blocked (red), working (amber), or populated/ready (dim). Empty and landed-only
workspaces render no token. The right-panel section remains the detailed list+
detail view and remains the activation destination for the token.

The implementation must keep the word `workspace` in code. THE-10's
workspace→project rename is explicitly out of scope.

## Scope audit and current seams

The existing draft change was read first: `proposal.md`, `design.md`,
`tasks.md`, and `specs/merge-queue-ambient-surface/spec.md`. It is useful as a
behavior sketch, but it is not authoritative. The following facts are verified
on this branch:

| Concern            | Current seam and consequence                                                                                                                                                                                                                                                                                                      |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Queue vocabulary   | `crates/thegn-core/src/attention.rs:351-409` owns `MqStatus`, parsing, and the capability-resolved `glyph()` mapping. Reuse it; do not add status strings or glyph literals.                                                                                                                                                      |
| Queue hydration    | `crates/thegn-host/src/attention_status.rs:170-185` already reads and parses all queue rows into `SidebarStatus::mq`; `crates/thegn-host/src/hydrate.rs:2949-2960` already hydrates the scoped panel list. No query, ticker, wake source, or provider is needed.                                                                  |
| Sidebar data       | `SidebarRow` is the shared paint/hit model (`crates/thegn-host/src/sidebar.rs:213-305`); worktree MQ status is denormalized at `:1270-1292`. Add a workspace-only `mq_rollup` there, derived from its child worktree rows.                                                                                                        |
| Sidebar paint/hits | `build_sidebar` and `hit_rows` share one placement pass (`crates/thegn-host/src/sidebar_view.rs:694-733`, `:1197-1233`), while workspace composition is at `:1490-1565`. Extend that shared placement with the token's x-span; never infer hit geometry from a second layout.                                                     |
| Right panel        | `Section::MergeQueue` already exists in `crates/thegn-host/src/panel/mod.rs:110-175`, and its list/detail renderer is `panel/sections/merge_queue.rs:53-224`. This issue activates that section; it does not add another panel section or duplicate its rows.                                                                     |
| Bottom bar         | `statusbar_items` unconditionally calls `push_mq_badge` (`crates/thegn-host/src/chrome.rs:1558-1614`), and the badge uses duplicated status literals plus a `⧉` draw-site literal (`crates/thegn-host/src/statusbar_badges.rs:137-196`). Make `mq` an ordinary opt-in `[bars]` widget and remove only the default badge emission. |
| Keyboard/mouse     | Workspace context menus are assembled in `crates/thegn-host/src/handlers/sidebar_keys.rs:382-433`; menu actions dispatch through `:1170-1277`. Mouse clicks use `handlers/sidebar_mouse.rs:203-298` and the same `RowHit`. Add an “Open merge queue” menu entry and a token hit path, not a new global key.                       |
| Render gate        | `render_plan` maps chrome/model damage to `Full` (`crates/thegn-host/src/render_plan.rs:88-99` in the architecture contract and the implementation's existing `damage.chrome` path). Queue-state changes must continue through the existing model/sidebar dirty path; do not weaken this to a sidebar-only partial paint.         |

The draft's claim that workspace token state should use `SidebarStatus::repo_scope`
is cut: `repo_scope` is the active attention scope, not workspace membership.
The correct membership is the `Group.path` set already emitted into each
workspace's rows by `build_rows`; this also covers dormant workspaces and avoids
cross-repo leakage. The draft's “no new panel work”, existing hydration, and
opt-in `mq` widget conclusions are already satisfied in substance and are
retained.

## Core policy

Add a focused, substrate-free module
`crates/thegn-core/src/merge_queue_view.rs`, registered from `lib.rs`. It owns
the pure policy and unit tests, not termwiz segments or terminal colors:

- `MqTier::{Blocked, Working, Populated}` and `MqRollup { tier, count }`.
- `rollup(statuses)` applies the existing priority: blocked statuses are
  `deferred`, `gate_failed`, `gate_error`, `needs_human`; working statuses are
  `folding`, `verifying`, `agent_running`; populated statuses are `queued`,
  `ready`; `landed` and unknown/unparsed values contribute nothing. If several
  tiers exist, retain only the highest tier and count entries in that tier,
  matching the current statusbar badge semantics.
- `MqTokenFit::{Full, MarkerOnly, Hidden}` plus `fit_token(available,
full_width, marker_width)`. The policy is: full `count + marker` first,
  marker-only second, hidden last. The host supplies measured display widths;
  core does not know Unicode, glyphs, colors, or terminal width.
- A semantic urgency/tone result for the rail (`None`, `Amber`, `Red`) may be
  represented by the tier mapping, but the host maps it to `Tok::Hue`; core
  must not import host palette types.

Tests cover every status bucket, priority/count behavior, empty and landed-only
inputs, and all width-fit boundaries. The host adapter chooses the existing
capability glyphs (`active_glyphs()` / `MqStatus::glyph` vocabulary) and maps
the semantic tier to palette tokens. No new literal glyph or color is allowed.

## Workspace token layout

`SidebarRow::mq_rollup` is `Option<MqRollup>` and is populated only for
`RowKind::Workspace`. During the existing denormalization pass, first read the
already-assigned child `mq_status` values (or the same `status.mq` map keyed by
child `worktree_path`) for matching `workspace_slug`, then call core
`rollup`. This is a projection only: DB ownership, attention ranking, and the
queue state machine do not move into the sidebar.

Create `crates/thegn-host/src/sidebar_mq.rs` as the host adapter. It should
produce the token segments and a measured hit span from one `MqRollup`:

- red blocked, amber working, dim populated;
- count before the tier marker; on narrow widths drop the count before the
  marker, then hide the token;
- use `caps::active_glyphs()` and palette `Tok` values only;
- never paint a token in rail mode. The rail may tint the existing workspace
  initial red for blocked or amber for working; populated stays neutral. This
  preserves the rail's existing “initial + activity” contract and avoids
  introducing a second rail row.

In `compose_row_lines`, the right cluster is ordered `mq token → warm token`
and is emitted as a `Line::Split` whenever either exists. The left workspace
label remains the identity side. The adapter's measured span is stored in the
`SidebarPlacement` and copied to `RowHit`, so the mouse target is exactly the
painted token even when `Line::Split` truncates the left label. The token never
gets its own row or changes header height. A 12-column full sidebar therefore
keeps the existing label/caret floor and drops token detail in the specified
order.

## Activation and reachability

Clicking the token on a workspace row returns a dedicated
`SidebarOutcome::OpenMergeQueue { repo_path }`. The event-loop dispatcher:

1. activates/switches to the workspace using the existing target seam;
2. selects the existing Work tab and `Section::MergeQueue`; and
3. marks the existing model/chrome dirty state so the normal render gate emits
   `Full` where required.

The workspace context menu gets an `Open merge queue` entry with no new
bindable action id. Focus the row, press the existing `m` menu key, and choose
that entry with Enter. This preserves keyboard access without expanding the
global `ACTION_SPECS`, control schema, completion catalog, or capability
catalog. The existing panel keys and `open-merge-queue` action remain the
canonical direct routes.

The right-panel section's list, detail column, row actions, and all-workspaces
scope remain unchanged. The token is only a door to that existing detail
surface; it is not a second queue implementation.

## Bottom-bar policy

Remove the unconditional `BarBadge::MergeQueue` insertion. Keep the shared
queue rendering helper in `statusbar_badges.rs` only as the renderer for an
ordinary `BarItemId::Widget("mq")`, so `[bars] bottom_right = ["mq"]` restores
the compact queue indicator. Its detail activation routes through the existing
`Widget` detail path to `unified_detail`, whose Merge queue block already reads
`model.panel.merge_queue`.

Document `mq` in the existing built-in widget list in
`crates/thegn-core/src/config.rs` and `config/config.toml.example`. This is not
a new config key: the array already accepts widget ids, the default remains
unchanged and excludes `mq`, and therefore no new env-overlay ratchet entry is
justified.

## Ratchets, providers, and invalidation

- No provider seam or vendor call changes; this is a projection of local DB
  cache/model data.
- No new capability, control verb, action id, completion slot, or config key.
  Verify the capability catalog, control-schema snapshot, completion-slot
  ratchet, and env-overlay ratchet stay unchanged.
- Run the color/glyph literal ratchets. Any new visual code must use the
  existing caps/palette chokepoints; do not add allowlist debt. If a newly
  fixed pre-existing entry is removed, update the relevant shrink-only file in
  the same chunk; otherwise leave the ratchets byte-for-byte unchanged.
- Update the sidebar, bars, and merge-queue help pages in the docs chunk. No
  new help context or action id is introduced, so help-context/action ratchets
  should shrink or remain unchanged, never gain an unclaimed entry.
- Queue hydration remains off-loop and wake-driven as it is today. A changed
  `SidebarStatus::mq`/panel model is chrome/model damage; the existing
  `damage.chrome ⇒ Full` render-plan invariant is the required repaint path.

## Verification

Each coder runs only scoped checks from their chunk. The Lead must not run
`just test`, `just ci`, a full-workspace compile, or e2e in this pipeline.
Implementation tests should include core policy tests, sidebar token fit and
paint/hit span tests, workspace rollup tests, menu/click routing tests, and
opt-in widget/detail tests. No e2e snapshots are re-recorded here.

### Unverified e2e baselines

The following existing muse baselines are expected to move and remain
unverified until a deliberate e2e/update pass:

- `test/muse/snapshots/sidebar__focused/xterm__100x30__linux.txt`
- `test/muse/snapshots/sidebar__focused/xterm__160x40__linux.txt`
- `test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__40x12__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__80x24__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__100x30__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__160x40__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__200x50__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__40x12__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__80x24__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__100x30__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__160x40__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__200x50__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__80x24__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__160x40__linux.txt`
- `test/muse/snapshots/themes__storm#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__light#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__abyss#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__ember#styled/xterm__100x30__linux.txt`

Some fixtures may have no queue rows, so the exact changed subset is still
unverified; the list is intentionally conservative.

## Delivery order

Chunk 1 lands the core policy and workspace-row projection. Chunks 2 and 3 are
file-disjoint and may then run in parallel: chunk 2 owns sidebar paint/hit and
activation; chunk 3 owns the opt-in bar, detail bridge, and documentation.
Each coder commits early and uses the exact subject specified in their chunk.
