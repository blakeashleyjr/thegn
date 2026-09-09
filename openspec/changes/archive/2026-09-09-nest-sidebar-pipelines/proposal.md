# Nest sidebar pipelines under their project

## Why

The sidebar grew a second, top-level pipeline section that competes with the
per-project `Pipelines` folders it was meant to complement. Two rows at depth 0
cause it:

- a `Pipelines` group at the tail of the whole tree, holding lanes thegn could
  not attribute to a project — and, in the flat layout, holding _every_ lane;
- a `Pipeline ▸ N running` roster rollup beside it, whose only job is to open
  the pipeline board.

On a real tree the effect is that "pipelines" appears both inside projects and
again at the root, so the reader has to check two places for the same thing.
The rollup is redundant besides: `Alt b` already opens the board from anywhere.

The attribution that dumped lanes into the tail group is also weaker than it
needs to be. It resolves a lane's project only through the live session groups
and the DB-registered worktrees, so a pipeline worktree that exists on disk but
is not yet registered — the ordinary case right after a pipeline creates one —
has no project and falls out to the root.

## What Changes

- **A `Pipelines` folder only ever hangs under a project.** The tail group is
  removed. There is no top-level fallback.
- **Attribution gains a sibling-directory fallback.** When no worktree of a lane
  resolves directly, the directory holding the project's other worktrees names
  the project. A directory two projects share is ambiguous and is dropped rather
  than guessed.
- **A lane no project claims is not emitted.** It stays on the pipeline board,
  which is the complete view of the roster; its worktrees, if registered, already
  have their own rows higher up the tree.
- **The flat layout emits no pipeline rows.** It has no project rows to nest
  under, and there is no longer a root group to fall back to.
- **The `Pipeline ▸ N running` rollup row is removed**, along with
  `RowKind::PipelineSummary`, the `SidebarRow::pipeline` field,
  `SidebarStatus::pipeline` and the `monitor_pipeline::summary` fold that fed
  it. `Action::OpenPipelineBoard` and its `Alt b` binding are untouched.

## Impact

- **Specs:** `sidebar` — one ADDED requirement pinning the nesting rule.
- **Code:** `sidebar.rs` (attribution + both depth-0 emissions), `sidebar_view.rs`
  (the two `PipelineSummary` paint arms), `handlers/sidebar_mouse.rs` and
  `handlers/sidebar_keys.rs` (the synthetic board action on the row),
  `monitor_pipeline.rs`, `attention_status.rs`, `docs/help/sidebar.md`.
- **Behaviour lost:** the at-a-glance running count in the sidebar. Deliberate —
  the board carries it, and a permanently-reserved root row for a feature many
  users never touch was the wrong trade.
- **Keybinds/actions:** none added or removed; the help ratchets are unaffected
  because `open-pipeline-board` keeps its id and its `docs/help/pipeline-board.md`
  claim.
