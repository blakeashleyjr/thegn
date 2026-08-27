---
id: pipeline-board
title: Pipeline board
parent: workflows
order: 6
actions: [open-pipeline-board]
---

# Pipeline board

A live picture of every agent thegn has dispatched, laid out **left to right**
as the pipeline it belongs to: one column per `[[pipeline.stages]]` entry, in
the order they are declared, with each stage's work under it.

`Alt b` opens it from anywhere, and pressing `Alt b` again closes it. You can
also run **Pipeline board** from the command palette, or press `↵` (or click)
on the **Pipeline** row the [[sidebar]] grows while any dispatch is live.

If nothing has ever been dispatched and no pipeline is configured, the board
says so in the status bar instead of opening an empty box.

## Reading it

The top two rows are the **stage rail**. The first names each stage and how
many of its rows are live against that stage's `concurrency` — `code 2/3` means
two workers running where the config allows three. The second names the agent
configured to run the stage and points a `→` at whatever its `next` is. That
arrow chain is the org chart: it is what makes the board read as a pipeline
rather than as a row of unrelated buckets.

Every configured stage gets a column **even with no rows in it**, so a stage
nobody has reached yet is visibly waiting rather than missing. Stages that show
up on the roster but aren't in your config follow, by name; dispatches made
outside a pipeline (the `D` key, a hand-run agent) group last under
`unstaged`.

Below the rail, each row is one dispatch: a status mark, the worktree it works
in, and how long it has been going.

- A `→` at the left of a row means its **parent is the stage to the left** — an
  architect's coder, say. The parent carries a matching `→` at its right edge,
  so the flow reads in both directions.
- Rows fanned out inside a single stage hang off **tree connectors** under the
  row they came from.
- An age drawn in red means the row has been live longer than its stage's
  `timeout_secs`. That is a **cue only**: nothing in thegn fires that timer, and
  nothing here advances, starts or stops a stage. Stage transitions belong to
  whatever is supervising the run — the board is a view of the roster, never a
  controller of it.

On a terminal too narrow to give every stage a readable column, the board
**stacks**: the same stages, the same rows, one group after another down the
page, with more room for each row.

## Keys

| Key               | What it does                          |
| ----------------- | ------------------------------------- |
| `↑` `↓` / `k` `j` | move the row cursor within a stage    |
| `←` `→` / `h` `l` | move to the neighbouring stage column |
| `↵`               | open the selected row's worktree      |
| `Space`           | freeze / unfreeze the live view       |
| `x`               | hide / show finished rows             |
| `Esc` / `q`       | close                                 |

The footer legend names the letter form of each of these; the arrow keys are
aliases for `hjkl`. Anything the board does not bind falls through to your
normal keybindings, which is why `Alt b` toggles it shut and `Ctrl-g` still
toggles the key lock.

Clicking a row selects it; clicking the selected row again opens it. The mouse
wheel scrolls, and a click outside the box closes the board.

## `↵` — going to the work

`↵` on a row lands you in that dispatch's worktree, exactly as pressing `↵` on
its [[sidebar]] row would, and closes the board. If the worktree isn't open as
a tab but is registered here, this **switches to its workspace and opens it**;
only a worktree thegn has no record of at all reports a miss in the footer.

## Freezing and hiding

`Space` freezes the **view**, not the pipeline: the roster keeps moving
underneath, so unfreezing shows the current truth rather than a gap. While
frozen the board also stops re-reading the roster at all.

`x` hides rows whose work has finished (merged, done, abandoned, failed). The
stage headers keep counting every row, so a stage never under-reports itself
while you are looking at a filtered view.

Neither toggle is remembered between openings. Both are reading postures for
the session you are in, not preferences — a board that reopened with finished
rows hidden would hide the thing you came back to check.

## Cost

The board re-reads the roster **only while it is open and unfrozen**, off the
UI loop, a couple of seconds apart. Closing it stops that entirely. A change
made elsewhere — an agent pane exiting, a dispatch recorded by a supervising
agent through `thegn dispatch put` — still reaches an open board immediately.

See [[configuration]] for the `[[pipeline.stages]]` keys, and
[[workflows]] for what the stages are for.
