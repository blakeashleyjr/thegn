# Design — pipeline board

## Decisions

### Adoption is a bounded plan over the existing attach path

The compositor claims `adopt_session` intents and plans each result before
performing side effects. Fresh, valid, not-already-visible sessions targeting a
resident worktree are attached through the same daemon-backed pane path used by
warm reattachment. Stale, malformed, duplicate, and unreachable intents are
claimed and discarded with an actionable status where appropriate. `focus:
false` remains the default and a focused batch changes groups at most once.

This keeps adoption idempotent at the UI boundary and prevents an old mailbox
from erupting into panes at startup.

### The board is a standalone boxed overlay

The accepted surface is `Option<PipelineBoard>` in the compositor, opened by
the `open-pipeline-board` action. It is not a `MonitorTab` and does not share
the system-monitor tab strip. That change from the proposal's first design was
intentional: the board owns stage-oriented navigation, help, footer facts, and
layout without coupling those concepts to hardware/process monitoring.

The overlay owns only presentation state. Dispatch ID, rather than row offset,
is the selection identity, so refresh and reflow preserve the selected record
when it still exists.

### Layout is pure and width-adaptive

The layout module derives ordered stage groups and board geometry without I/O.
At usable wide widths, configured stages appear as columns; at narrow widths,
the same groups stack vertically. Empty configured stages remain visible so the
board can communicate pipeline shape. Unconfigured and unstaged roster records
remain representable rather than disappearing.

Each row presents bounded facts already available from config or the roster:
status glyph, stalled cue, agent, concurrency, issue/artifact, age, and next
stage. Rendering does not advance a stage or enforce the displayed limits.

### Activation reuses worktree navigation

Enter on a dispatch row constructs the same worktree target used elsewhere in
the UI. If the worktree is resident, activation uses its current row. If it is
dormant and still exists on disk, activation can materialize it through the
shared fallback. Failure is surfaced instead of silently dropping the action.

Pane/session-level targeting remains outside this accepted scope.

### Roster hydration is off-loop and visibility-gated

Roster reads run in blocking work and return through the existing refresh and
waker path. The loop takes one initial sample and samples again when the roster
is marked dirty. The two-second cadence runs only while the board is open; the
closed board adds no periodic poll and no wake source. Stale or unchanged
results do not create unnecessary work.

Damage follows the surface actually affected:

- with the board closed, changed derived stage evidence can produce a
  sidebar-scoped incremental frame, otherwise the frame can be skipped;
- with the boxed board open, a changed board sample requests `Full`, matching
  the compositor's existing overlay rule;
- pane output remains pane-scoped when no overlay requires a full frame.

The earlier blanket “never Full” wording was therefore inaccurate for the
accepted standalone overlay and is deliberately removed.

### Timestamp normalization happens at the boundary

Historical dispatch rows may store Unix seconds while current rows store Unix
milliseconds. Reads normalize plausible second-based values before age and
stalled calculations. New writes remain milliseconds. This avoids spreading
unit heuristics across render code.

### Sidebar composition is separately owned

This change publishes the current stage as derived worktree evidence. The
accepted project-nested presentation and removal of the old root summary belong
to `nest-sidebar-pipelines`; archiving this change does not duplicate that
contract.

## Evidence

- `crates/thegn-host/src/run.rs` owns the optional board, open action, gated
  sampling, dirty refresh, and activation plumbing.
- `crates/thegn-host/src/pipeline_board/mod.rs` owns runtime board state and
  dispatch-ID selection.
- `crates/thegn-host/src/pipeline_board/layout.rs` owns column/stacked layout.
- `crates/thegn-host/src/render_plan.rs` locks the open-overlay and closed-board
  damage behavior.
- THE-74 artifacts record the accepted v2 design; commits `42984dc4`,
  `9bc47777`, `c037cbab`, and `cb34db84` contain the implementation and review
  fixes.
