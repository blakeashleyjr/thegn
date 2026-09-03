---
id: sidebar
title: Sidebar
order: 3
contexts: [zone:sidebar]
actions:
  [
    focus-sidebar,
    toggle-sidebar,
    move-item-up,
    move-item-down,
    move-worktree-to-folder,
    toggle-region,
    sidebar-narrower,
    sidebar-wider,
  ]
---

# Sidebar

The left tree: every project, its worktrees, and your standalone
terminals. `Alt-s` (or `Ctrl-←` from the leftmost pane) focuses it;
`Ctrl-Alt-s` cycles it through three states: **full → rail → hidden**. The
rail is a slim strip that keeps each row's activity dot and initial visible
while reclaiming the columns; press `Alt-s` to grow it back into the full
tree. `q` or `Esc` returns to the terminal.

## Reading the tree

The sidebar is tiered so a repo and its drawers never read the same. Each
**project** header is the loudest row in the column — bold, in the accent
color, marked `◆` (`⌂` for a plain directory, `≡`/`⇅` for a terminal-host
group) and sitting on its own recessed band. **Folder** headers are
deliberately quieter: plain text, a faint `▪`, and the filed count grayed —
a drawer inside the repo, not a repo itself. Worktree and terminal rows are
the body of the tree.

Each project's block — its header, its worktrees and its folders — shares one
background tint, and consecutive projects alternate between two, so two open
repos never run together. The header band stays the darkest row either way, so
it still reads as the start of a block. `[ui] sidebar_dividers = false` turns
the alternation off for one uniform tint. This used to be a blank separator row
above each header, which cost a screen row per repo; the tint says the same
thing for nothing, so unlike the gap it also applies in the rail and while a `/`
filter is active.

When a project has active merge-queue entries, its full-mode header shows a
compact token immediately before the warm-pool token: the count followed by a
red blocked, amber working, or dim populated marker. Landed-only and empty
queues stay quiet. The token is a shortcut to Work ▸ Merge queue; click it, or
choose **Open merge queue** from the project's `m` menu. Narrow headers drop
the count before hiding the marker, and the token is never painted in rail
mode.

## Navigate

- `↑↓` / `j` `k` — move; `↵` opens the row (or folds a header; on the
  empty TERMINALS hint it creates a terminal)
- `PgUp` `PgDn` — move by ten rows; `Home` `End` — first / last row
- `←` / `h` — fold a header (from a row inside one: jump to the header);
  `→` / `l` — unfold
- `/` — filter the tree (`↵` applies it; the next `Esc` or `q` clears it)
- `Alt-1..9` / `Ctrl-1..9` — jump to worktree / project by slot. The
  `Ctrl-` digits need a terminal that reports modified keys; when it does
  not, thegn stops painting those digits — see [[terminal-compatibility]]
- `Alt-\`` — bounce between the projects and terminals regions
- `q` or `Esc` — back to the terminal

## Create

- `n` — new worktree in the project under the cursor (in the TERMINALS
  section: a new terminal)
- `N` — new project
- `b` — branch a new worktree off the one under the cursor

## Organize

- `f` — move to a folder (or create one)
- `r` / `F2` — rename
- `p` — pin to top
- `s` — sort menu: manual / name / recent / attention / live (live orders
  worktrees by most-recent process/agent activity, newest first). The
  active mode is always on screen: it is the quiet word at the right of
  the `WORKSPACES` header, which you can also **click** to open this menu.
  The header drops the word rather than clipping it in a narrow column.
- `Space` — mark rows for bulk actions (marks clear when you click or type
  into a pane); `Shift-↑↓` — reorder manually

## Reorder

`Shift-↑↓` moves the row under the cursor (or every marked row, as a
block); `Ctrl-Alt-↑↓` moves one item and works from anywhere.

A worktree moves within its **run** — the loose list under its project,
or the folder it is filed into. Push it past the end of a run and it
crosses into the next one, leaving or joining that folder as it goes: one
key both reorders and re-files. `home` is anchored at the top of the loose
list; it never moves and nothing lands above it. A collapsed folder is
stepped over rather than entered, so a worktree can't vanish into a folder
you have closed.

Put the cursor on a folder header to reorder the folder itself among its
project's folders — its worktrees travel with it. Project headers and
terminals reorder among their own kind the same way.

Manual reordering implies the manual sort, so a move under a computed sort
(`s`) switches back to manual first. Order is saved per project and
restored on the next launch.

## Mouse

Mouse support turns on only when the outer terminal reports it; every
gesture below has a keyboard equivalent.

- **Click** selects the row (and opens a worktree or terminal; headers just
  select); clicking the caret glyph folds or unfolds a header instead
- **Double-click** moves keyboard focus into the pane (or folds a header)
- **Ctrl-click** marks a row for bulk actions, like `Space`
- **Right-click** opens that row's action menu, anchored under it; the menu
  then takes clicks and the wheel until you pick an entry or click away
- **Wheel** scrolls the tree, walking the selection with it (and focuses
  the sidebar, like a click)
- **Drag** a worktree to reorder it. Release **on** a row to put the dragged
  worktree in that row's place — the row you drop on moves aside. Drop on the
  **last** row to land at the end. This works _inside_ a folder too, which
  files and positions it in one go; drop on a folder header to file it at the
  end of that folder, or on its project header to move it back out. Drag a
  folder header to reorder folders, or a project header to reorder
  projects. Dragging across projects is refused, `home` stays anchored at
  the top, and the insertion rule shows exactly where a release will land.
  Releasing anywhere inside the sidebar lands on the nearest row — the blank
  tail below the list puts the row at the end — and a release outside the
  sidebar cancels. `Esc` abandons a drag without moving anything.

## Pipeline lanes

Inside every project whose worktrees a pipeline has spawned, the tree grows a
**`Pipelines` folder** — one per project, at the tail of that project's own
tree — holding one folder per pipeline, named from the roster's issue id:

```
▾ app
    home
    other
  ▾ Pipelines (2)
    ▾ THE-74 (2)
        tg-the-74
        tg-the-74-review
    ▸ THE-9 (1)
```

Every worktree the pipeline's roster rows reference hangs inside its lane
folder. `↵` (or a double-click, or a click on the caret) folds and unfolds a
group or a lane; `↵` on a worktree opens it, exactly as opening it from its
normal row would — including switching project when the worktree belongs to
another one.

These folders are **derived, not real folders**. They come from the dispatch
roster — its rows of **any** status, not only the live ones — so they survive a
restart and a finished lane stays until its rows are removed from the roster.
A worktree no roster row references stays exactly where it was. A pipeline
folder only ever hangs under a project: a lane whose worktree is not registered
anywhere is filed by the directory its siblings live in, and one thegn cannot
tie to any project at all is left out of the tree rather than opening a second,
top-level pipeline section — the pipeline board (`Alt-b`) is the complete view
and still lists it. For the same reason the flat layout, which has no project
rows to nest under, grows no pipeline rows. So a lane cannot be renamed,
reordered, pinned, marked or filed
into, and the same worktree shows up once per lane that references it — the
worktrees' own rows higher up the tree are untouched, a lane shows a second
view of them, never a second identity.

## Activity dots

The dot on the **left** of a worktree row is its activity; the amber dot on
the far right is a separate thing (uncommitted changes). Reading left to
right through a turn: white while the agent works, then amber or red when
it wants you, hollow once you've looked.

| Dot           | Means                                                                             |
| ------------- | --------------------------------------------------------------------------------- |
| `●` white     | working — processes under the worktree are busy, or its agent is producing output |
| `●` amber     | finished, and you haven't looked yet                                              |
| `●` red       | **blocked on you** — the agent asked something, or a queue needs a human          |
| `○` amber/red | seen: you focused the tab, but it's still waiting                                 |
| `↻`           | the worktree is being built                                                       |
| `✗`           | its environment failed to come up                                                 |
| _(none)_      | nothing to report                                                                 |

Two things the dots deliberately do **not** do:

- **A plain terminal never goes red.** Only a worktree with a real agent
  can ask for you. A shell that ran a build shows white while it runs, then
  goes back to no dot.
- **A dot never turns red on one quiet moment.** An agent thinking at ~0%
  CPU still counts as working, so the alert needs a sustained quiet stretch
  — it won't flicker mid-turn.

A red or amber dot is sticky: focusing the tab makes it hollow (seen) but
does not clear it. It only goes back to white when work genuinely resumes.
Thresholds are tunable — see [[config-reference]] `[activity]`, and
`[theme.colors]` for the three dot colours.

Git state on the right keeps submodules separate from ordinary file dirtiness.
The submodule marker means a gitlink is moved, dirty, untracked, conflicted,
uninitialized, or otherwise unavailable; it does not turn on the ordinary
dirty dot. Set `[ui] sidebar_show_submodules = false` to hide only this marker.
The setting is trusted user/workspace configuration and cannot be supplied by
repo-local `.thegn.toml`.

## Act

- `d` / `Del` — close or delete… (deleting files from disk is always the
  explicit second choice, never the default)
- `c` — copy the worktree path
- `m` — the full row action menu; on a worktree row choose **Open in IDE** to
  hand that exact worktree over without first switching to it. Dormant rows are
  valid for a windowed IDE; a terminal-only editor asks you to focus the row.

## View

- `<` / `>` (or `,` / `.`) — resize the sidebar, 2 columns a press; `e` —
  wide mode (`Esc` collapses it back)
- `?` — this page
- `g` — flat / grouped: toggle between one list of every worktree across
  all repos (each tagged with its repo, ordered by the current `s` sort)
  and the per-project grouping. Pair with the `s` → live sort to always
  see the latest-changed worktree at the top, regardless of project
- `i` — row detail: cycle the secondary line (branch, ahead/behind, PR)
  between **all** rows, the **cursor** row only, and **off**. The detail
  line only ever shows while the sidebar has focus. Defaults to the cursor
  row — **all** doubles every worktree row's height on focus, so it halves
  how much of the tree fits. This overrides
  `sidebar_focus_detail` from [[config-reference]] `[ui]` and persists

Project ordering is configurable: `sidebar_project_sort = "attention"`
bubbles the project that most needs you to the top. See
[[config-reference]] `[ui]`.

**Attention and live sorts hold still while you navigate.** Both re-rank
off a signal that changes on its own (tier ranks, last-active time), so
without this a row could move out from under the cursor mid-keystroke.
The order is held for as long as the sidebar has focus — the header chip
appends `hold` while it is — and for a couple of seconds after an
`Alt-↑↓` / `Alt-1-9` jump made from a pane. It re-settles the moment
focus moves into a pane, and a worktree created while held still sorts to
its real position rather than the bottom. Turn it off with
`sidebar_freeze_sort = false` in [[config-reference]] `[ui]`; the other
three sorts never move on their own, so it does nothing there.

## Width

Four ways to set it, all persisted, all writing the same width:

- `<` / `>` (or `,` / `.`) — 2 columns a press, while the sidebar has focus
- `Ctrl-Alt-,` / `Ctrl-Alt-.` — the same nudge from any zone, so you can
  widen the tree without leaving the terminal you are typing in
  (`sidebar-narrower` / `sidebar-wider`, rebindable, and in the `Ctrl-k`
  palette)
- **Drag the separator** — the grab takes the divider **or the pane edge
  beside it** (the two read as one boundary), and the divider keeps its grab
  offset so it stays under the cursor instead of jumping to it; release
  commits. A press that never moves changes nothing, and `Esc` cancels a
  resize, restoring the width you started with
- `sidebar_width` in [[config-reference]] `[ui]` — the resting width a
  fresh install starts at. Any of the three above beats it from then on

The floor is 12 columns and the ceiling is ~half the window — the same
ceiling `e` (wide mode) expands to, tunable with `sidebar_wide_ratio`. The
status line reports the width you land on, so a nudge that hits the clamp
says so rather than reading as a dead key.

Setting a width while in wide mode drops you out of it, so the width you
asked for is the width you get — that holds for the nudge and the drag
alike.

Width belongs to the **full** tree. In rail mode `<` / `>` and the drag are
refused with a pointer instead of quietly persisting a width you would only
discover on the next restart; press `Ctrl-Alt-s` to grow the rail back
first. On a window too narrow for the sidebar (under 76 columns) the tree
auto-hides whatever width you set — the rail survives.
