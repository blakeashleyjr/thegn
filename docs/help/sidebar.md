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
    open-info,
    mark-unread,
  ]
---

# Sidebar

The left tree: every project, its worktrees, and your standalone
terminals. `Alt-s` (or `Ctrl-←` from the leftmost pane) focuses it;
`Ctrl-Alt-s` cycles it through three states: **full → rail → hidden**. The
rail is a slim strip that keeps each row's activity dot and initial visible
while reclaiming the columns; press `Alt-s` to grow it back into the full
tree. `q` or `Esc` returns to the terminal.

Workspace headers show the workspace's zone after its name (`zone work`).
`zone ?` means the zone catalog could not be read; a trailing `!` (amber)
means the zone has no `[zone.<name>]` policy in your config. The chip drops
first when the header is too narrow, and the slim rail never shows it.

While the sidebar is hidden, the masthead shows a `◌ sidebar hidden` chip
(`◌` alone on a narrow terminal, `.` under ASCII glyphs) after the brand, so a
hidden sidebar is never invisible. Clicking it cycles the sidebar back to the
full tree, the same as `Ctrl-Alt-s`.

The other two states say which one you are in, too. In the full tree the
`WORKSPACES` header carries a **mode chip** left of the layout and sort words:
`full·focus`, `full·always` or `full·never`, naming the full mode and where the
row detail line shows (on the cursor row, on every row, or nowhere: the `i`
toggle, else `[ui] sidebar_focus_detail`). It is the first thing on the header
to give way in a narrow column: the chip shrinks to the bare word `full`, then
goes. On a two-line row the detail line continues the tree guide, a dim `│`
under the row's own `├` (blank under the last `└`). The rail's top row shows
the `▏` glyph (`|` under ASCII) above its entries. **Clicking** the chip or
the rail glyph cycles the sidebar **full → rail → hidden**, exactly like
`Ctrl-Alt-s`.

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

A worktree deleted outside the running TUI (`wt rm`, `wt prune`, a landing or
plain git) leaves the bar without a restart. A leftover directory with no
checkout in it is not a live worktree, so it gets no row.

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
- `u` — mark unread: turn a hollow (seen) dot back to its filled amber/red
  state until you focus the tab again. Acts on every marked row, else the
  cursor row (on a batch row, every member of the batch); a row with no dot
  is left alone. Also in the row menu and the palette ("Mark worktree
  unread"); from a pane the palette action marks the focused tab, which
  stays filled until you leave it and come back.
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
- **Click** a `… show N more` / `show fewer` row to expand or truncate its
  group (see Long groups below); the header mode chip and
  the rail's top glyph cycle the sidebar full → rail → hidden
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

## External worktrees: the Other group

A project's worktrees include every git worktree of the repository, so ones
another tool made (an agent harness, a script, plain `git worktree add`) show
up too. A checkout thegn is itself creating — the new-worktree wizard prepares
its worktree while the form is open — is never picked up from here: it appears
only when its creation completes, and cancelling the wizard leaves no row
behind. A worktree is **external** when it sits outside thegn's own checkout
layout (`<worktrees_dir>/<repo>/…`, or `.worktrees` inside the repository),
outside every `worktree_roots` entry of its project, and has not been filed
into a folder — filing one adopts it. The home checkout is never external.

By default each project gathers its external worktrees under one collapsible
row after its folders:

```
▾ app
    home
    feature-a
  ▸ Other (4)
```

`↵`, `←`/`→`, a click on the row or on its caret folds and unfolds it, and the
choice is remembered per project. Like the `Pipelines` folder it is derived:
it cannot be renamed, pinned, marked, dragged or filed into. The worktree you
are in is never hidden — while it is external, its Other group stays open.

Three `[ui]` settings in [[config-reference]] shape it:

- `sidebar_external_worktrees` — `group` (the default) gathers them under the
  Other row; `show` lists them among the project's own worktrees; `hide`
  leaves them out of the tree (and the flat list) entirely, except the active
  one.
- `sidebar_other_group_label` — the row's label (default `Other`); it reads
  `Other (N)`.
- `sidebar_other_group_collapsed` — whether the row starts collapsed (default
  `true`) until you toggle it.

To count a directory's worktrees as the project's own, list it under the
project: `[project.<slug>] worktree_roots = ["~/agent-worktrees"]`.

## Long groups: show more

A group with many entries is cut to its first rows and closed by one
highlighted toggle row at the group's indent, which also closes the group's
guide line:

```
  v wip (20)
      |- feature-01
      |- feature-02
      ...
      |- feature-07
      +- ... show 13 more
```

`Enter`, `Space` or a click on that row expands the group, and the row then
reads `show fewer`; the cursor stays on it either way. The choice is
remembered per group across restarts. Groups are a project's own worktree
rows, each folder, the Other group, the `Pipelines` folder and each pipeline
lane.

- Protected rows are never hidden: the worktree you are in, the row under
  the sidebar cursor, pinned rows, rows that need you (blocked attention or a
  failed run) and any batch row that holds one of these. They count toward the
  limit, so a protected row past it takes the place of the last unprotected
  one. A row that finished unread is not protected.
- Everything else folds from the tail, in display order.
- An open batch row is not limited: it shows every lane it holds.
- While a `/` filter is active nothing is truncated: every match shows.
- The toggle row cannot be pinned, marked, dragged or filed, and has no menu.

Two `[ui]` settings in [[config-reference]] set the limit:

- `sidebar_group_row_limit` - entries a group shows before the toggle
  (default `7`); `0` turns truncation off.
- `sidebar_group_row_limits` - per-group overrides keyed by the group's label
  (a folder name, the Other label, a project name):
  `sidebar_group_row_limits = { Other = 5, "agents wip" = 0 }`. `0` turns it
  off for that group alone. Overrides match by group label across all
  workspaces, so same-named folders in different workspaces share one.

## Batch rows

Agent lanes fold harder than ordinary worktrees. A loose worktree belongs to
a batch when its label carries a batch token: take the part after the last
`/`, up to the first `-`; it must be one to three letters followed by one to
six digits, and a `-` must follow. So `tg/b158-config-dir-env` belongs to
batch `b158`, while `tg/hardy-onyx`, `main` and `the-2060-x` belong to none.
Pinned worktrees never join a batch.

When at least `sidebar_batch_group_min` worktrees share a token (default `2`;
`0` turns batch rows off) they fold into one row:

```
  v app
      |- home
      |- b163 (3)   1 needs you
      |  |- spine
      |  |- view
      |  +- docs
      +- b158 (2)
```

- The row is collapsed by default. `Enter`, `Left`/`Right` or a click opens
  it, and the choice is remembered per batch. It also renders open while it
  holds the worktree you are in.
- Batch rows come after the single loose rows, newest first (the number
  descending, then the token). Members keep the workspace sort order and show
  their name without the batch prefix, so `tg/b161-b161-spine` reads `spine`.
- A batch row counts as one entry toward its parent's row limit. It cannot be
  pinned, marked, dragged, filed or renamed. A `/` filter reveals its matches
  like a folder.
- The label is bold when any member needs you and in the focus colour when it
  holds the active worktree. The count `(N)` is faint. The right side reads
  `N needs you` (`N need you` when more than one), `N active` and `N done`,
  clipped from the right in that priority order.

## One line when the second adds nothing

A worktree row's second line (branch, commits, PR, upstream) drops anything
the title line already says:

- the branch, when it equals the title;
- the commit count, when the title shows commit or behind-base badges;
- the PR, when the title shows the PR chip;
- the upstream, when the title shows ahead/behind.

If nothing is left the row is one line. A title counter is never repeated on
the second line either.

## Names, markers and the guide

The worktree name is toned by state, from strongest to quietest:

| State                                     | Name              |
| ----------------------------------------- | ----------------- |
| the worktree you are in                   | focus colour      |
| needs you (blocked or failed)             | normal text, bold |
| working, loading, finished unread, cursor | normal text       |
| seen or idle                              | dim               |

The marker glyph in front of the name carries the same state, so it reads
without colour: the glyph set differs per state, and needs-you is also the
only bold name. In the ASCII tier the markers are ASCII characters.

The tree guide is one continuous line. Ancestor pipes are drawn through
two-line rows, batch rows and the toggle row, and the toggle row closes the
group with a corner (`+` in ASCII). Folder, Other and `Pipelines` headers keep
their own layout and interrupt the line, since they open a new section.

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
      THE-9
    ▸ Finished (3)
```

A lane is named for its issue (or lane name) without a `local:` prefix. A lane
with a single worktree is one row, not a header plus a repeated child. Lanes
whose roster rows have all finished (merged, done, failed or abandoned) fold
into one `Finished (N)` group. After `sidebar_finished_pipeline_days` days
(default `7`; `0` = never) a finished lane leaves the sidebar; its worktree is
never deleted — `wt prune` does that. The open worktree's lane always stays.
**Clear finished** in the `Finished` row's menu hides every lane finished so
far in that project at once; a lane that finishes later shows up again. A
folder you made yourself named `Pipelines` does not become a second Pipelines
group: pipeline worktrees filed there show in the one `Pipelines` group.
Integration and release worktrees (`tg/bNNN-integration`, `tg/bNNN-release`)
are not pipelines: they sit with the ordinary worktrees. Worktrees filed under
`Merged` show their age and size, not the last commit subject.

Every worktree the pipeline's roster rows reference hangs inside its lane
folder. `↵` (or a double-click, or a click on the caret) folds and unfolds a
group or a lane; `↵` on a worktree opens it, exactly as opening it from its
normal row would — including switching project when the worktree belongs to
another one.

These folders are **derived, not real folders**. They come from the dispatch
roster — its rows of **any** status, not only the live ones — so they survive a
restart and a finished lane stays until its rows are removed from the roster.
Every active roster row is retained, along with the newest 2048 finished rows.
Older finished lanes drop out of the sidebar. A worktree no roster row references
stays exactly where it was. Finished roster rows (not review tasks) are deleted
90 days after they finish unless their worktree is still registered, an active
autopilot run uses them, or a retained child points to them, so a finished lane
of a removed worktree eventually leaves the sidebar. A pipeline
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
Press `u` on a hollow row to flag it again (filled, and ranked up by the
attention sort) until you next focus it.
Thresholds are tunable — see [[config-reference]] `[activity]`, and
`[theme.colors]` for the three dot colours.

Git state on the right keeps submodules separate from ordinary file dirtiness.
The submodule marker means a gitlink is moved, dirty, untracked, conflicted,
uninitialized, or otherwise unavailable; it does not turn on the ordinary
dirty dot. Set `[ui] sidebar_show_submodules = false` to hide only this marker.
The setting is trusted user/workspace configuration and cannot be supplied by
repo-local `.thegn.toml`.
The +/- line counts show a dim `bin` when the change includes binary files, and
`+capped`/`-capped` in place of a count when the diff cap is reached. If a diff
count cannot be read, the row keeps its last known counts; with no earlier
reading, it shows an amber `?`.

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
  see the latest-changed worktree at the top, regardless of project. The
  header always names the current layout (`grouped`, or `flat` in the accent
  colour) next to the sort chip. Click it to switch back: if your projects
  and folders have vanished into one long list, that is flat mode
- `i` — row detail: cycle `sidebar_focus_detail` between **all** rows, the
  **cursor** row only, and **off**. The default is **all**. Visibility outside
  sidebar focus is controlled separately by `sidebar_detail_when`; `i` cycles
  only `sidebar_focus_detail`. Both settings are in [[config-reference]] `[ui]`
  and the `sidebar_focus_detail` runtime choice persists.
- `I` — Info screen for the row under the cursor (also "Info screen"
  in the palette): identity, the effective settings with the layer that set
  each one (env, sandbox backend and what would launch here, profile, network,
  agent, merge gate), overrides, sessions and stats. Esc closes it. The row's
  context menu has the same thing as "Info…" (id `info`), for a workspace row
  and a worktree row.

### Row detail

Git status details refresh for visible, recently active, or live worktrees.
Other worktree rows keep their last-known status until they become active or
visible, so a large registered worktree list does not trigger background scans
of every checkout.

The richer detail line is configured by `sidebar_detail_when`,
`sidebar_show_commits`, `sidebar_show_behind_base`, `sidebar_commit_badge`,
`sidebar_behind_badge`, and `sidebar_detail_fields` in [[config-reference]]
`[ui]`. Commits count commits ahead of the repository's default branch; the
badge caps at `999+`. Valid field names are `branch`, `commits`, `dirty`,
`branch_stat`, `pr`, `subject`, `age`, `upstream`, `stage`, `activity`, `env`,
`sandbox`, `devcontainer`, `hibernated`, `mq`, `unread`, `disk`, `author`,
`stash`, `ci`, and `issue`.
The default field order is `branch`, `commits`, `dirty`, `branch_stat`, `pr`,
`ci`, `issue`, `author`, `stash`, `subject`, `age`, `upstream`, `env`,
`sandbox`, `devcontainer`, `hibernated`, `mq`, `unread`, `disk`; only `stage`
and `activity` are opt-in. Every default field is hidden while it has nothing
to show, so a row with no CI run, linked issue or stash looks as before. The legacy
`sidebar_detail_branch`, `sidebar_detail_branch_stat`, and `sidebar_detail_pr`
toggles remain AND gates; prefer `sidebar_detail_fields` for membership and
order. An entry can also be `plugin:<plugin>/<field>`; plugin IDs use
`[A-Za-z0-9._-]{1,64}` and fields use `[a-z0-9_-]{1,32}`. An absent plugin is
silently empty. An empty list keeps only the branch fallback. Each requested field is
rendered in the requested order, including environment, sandbox, devcontainer,
hibernation, merge-queue, unread, and disk.

Plugin fields whose manifest caps set `slot = "badge"` render as compact badges
after the commit badge. They are capped to 12 columns and are removed first
when the row is narrow, before the label is truncated. A badge still needs its `plugin:<plugin>/<field>`
entry in `sidebar_detail_fields` to be enabled.

The author field is read with the existing branch metadata scan and adds no
git spawn. The `stash` field shows `stash N` when the repository has stash
entries and is hidden when there are none or the count is unknown (for
example, on a remote worktree). It counts the stash reflog directly, so it adds
no git spawn either. The stash is shared by every worktree of a repository, so
each of them shows the same count.

The `ci` field shows the worktree's latest CI result as `ci pass`, `ci fail`
(in the error colour), `ci run`, or `ci pend`. It reads the cached latest run
that the attention scan already uses, so it adds no CI fetch or spawn, and it
can lag the live provider by the CI refresh cadence. It is hidden while no run
is cached, and `ci run` and `ci pend` are also hidden while the CI refresh for
that worktree is failing, because no live result backs them; a cached pass or
failure stays shown.

The `issue` field shows the newest linked issue id, +N when more are linked;
read from the local link table, no fetch. An all-digit id is shown as `#123`,
any other id as stored (for example `ABC-9 +2`). Control characters are
stripped, the id is clipped to 24 cells, and the field is hidden while the
worktree has no linked issue. The link table is read once per attention scan,
so a new link appears on the next scan.

When the row is narrow, elision drops content in this order: plugin fields,
activity, stage, upstream, author, stash, ci, issue, subject (first truncated to eight cells), age, tail badges
(disk, unread, merge queue, hibernation, devcontainer, sandbox, environment),
branch stat, PR, dirty, commits, then branch. The branch is always retained and
gets an ellipsis.
Age is approximate and is read when the line is composed; it does not tick
while the UI is idle.

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
