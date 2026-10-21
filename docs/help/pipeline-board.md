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
configured to run the stage, shows its effective model and source, and points a
`→` at whatever its `next` is. `entry: sonnet` means the agent entry supplied
the model; `stage: opus` means the stage overrode it; `model: none` means
neither supplied one. A resolution failure is shown as `model error`, never as
a blank. That arrow chain is the org chart: it is what makes the board read as
a pipeline rather than as a row of unrelated buckets.

Every configured stage gets a column **even with no rows in it**, so a stage
nobody has reached yet is visibly waiting rather than missing. Stages that show
up on the roster but aren't in your config follow, by name; dispatches made
outside a pipeline (the `D` key, a hand-run agent) group last under
`unstaged`.

The Lead that drives a chart is an agent running the `pipeline` skill, and a
fleet over a batch of issues is `supervise`. They and `mq` are entries in the
embedded [[skills]] registry, compiled into the binary and seeded per worktree
into each configured harness's native project directory. Their gates are
unchanged: `pipeline` is eligible once a chart has a stage, `supervise` is
always eligible, and `mq` requires the merge queue. Stage workers are launched
with `thegn session open --agent <entry> --stage <stage>`, which applies the
stage's `model` / `env` / `permissions` overrides (see [[configuration]]).

Below the rail, each row is one dispatch: a status mark, the worktree it works
in, and how long it has been going.

- A `→` at the left of a row means its parent row's configured stage `next`
  names this row's stage. The stage name beside the arrow labels the source,
  including when columns are not neighbours or appear out of transition order.
  The parent carries a matching `→` at its right edge, so the flow reads in
  both directions. Column adjacency alone never creates a flow; if the parent's
  stage is historical or unconfigured, no cross-stage edge is drawn.
- Each active row also shows its effective model and source using the same
  `entry:` / `stage:` / `model: none` labels. A resolution failure is shown as
  `model error`.
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
a tab but is registered here, this **switches to its project and opens it**;
only a worktree thegn has no record of at all reports a miss in the footer.

## Freezing and hiding

`Space` freezes the **view**, not the pipeline: the roster keeps moving
underneath, so unfreezing shows the current truth rather than a gap. While
frozen the board also stops re-reading the roster at all.

`x` hides rows whose work has finished (merged, done, abandoned, failed). The
stage headers keep counting every row, so a stage never under-reports itself
while you are looking at a filtered view. Roster reads retain every active row
and the newest 2048 finished rows; older finished lanes are dropped. When the
board hits that limit, its footer says "newest 2048 finished".

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

## Worktree setup in a pipeline

`[[pipeline.stages]]` is a structure-only roster: it validates and describes
the chart, but it does not create worktrees or execute lifecycle commands.
External supervisors should create worktrees with `thegn wt new` or the
existing `worktrees.create` control operation. Those doors use the shared
`pre_create`/`post_create` lifecycle, and the same `pre_destroy`/
`post_destroy` hooks apply when the worktree is later removed. There is no
pipeline-specific hook configuration.

`thegn wt folder <worktree> <name>` assigns a repo-local sidebar folder;
`thegn wt folder <worktree> --clear` unfiles it without deleting the folder.
Control clients use the Git-scoped `folders.assign` capability: pass exactly one
of `folder` (a name) or `clear: true`; neither, or both, is rejected. Over MCP
the `folders_assign` tool needs `--scopes git`.

## Finishing a stage

A worker reports with `thegn dispatch report <row>` and text that starts with
`verdict:` and `gate:` lines (see the pipeline skill). A PASS without a gate
result is not accepted as proof. Completion of a stage with an artifact is
refused while thegn cannot read the worktree's git status; retry when it can,
or record an explicit forced completion.

Before `thegn dispatch set-status <row> done`, the Lead runs
`thegn dispatch gate <row>` to record an attestation for the committed HEAD;
without a satisfied attestation, normal completion is refused. If a worker is
gone and its report claims PASS without an attestation for its HEAD, the reaper
returns the row as `needs-decision` for the Lead. A live Lead lease binds claims
and status changes to its owner; see [[cli]] for `--lease`, `--owner`, and the
audited `--admin --why` override. If the stage or effective agent configuration
changes between claim and launch, launch is refused and the row is marked
`failed` for retry.

Progress notes are limited to 2,000 notes and 2 MiB of text per dispatch row.
Further `thegn dispatch note` requests are refused with a pointer to
`thegn dispatch report`, and status JSON marks a truncated note list.

A pipeline row uses its own handoff file,
`.thegn/pipeline/<issue>/<stage>/<row>.md`. `thegn dispatch verify`,
`set-status done` and the wake handoff refuse a different artifact or an
empty committed handoff. Plain dispatch rows keep their existing behavior.

A chunk's `after:` names resolve only to rows in the same stage. The newest
attempt of each chunk name governs, and a done or merged row satisfies the
dependency. If same-stage rows make a name ambiguous, admission is refused
and names every matching row.

Dispatch identity fields reject terminal control and Unicode format characters.
`dispatch put` also refuses a missing, cross-issue or ineligible parent row;
the control API reports identity text as an invalid argument and parent
refusals as failed preconditions.
