---
id: terminal-and-panes
title: Terminal & panes
order: 4
contexts: [zone:center]
actions:
  [
    enter-replay,
    export-cast,
    toggle-recorder,
    new-tab,
    new-terminal,
    new-pane,
    launch-menu,
    split-down,
    split-right,
    close-pane,
    zoom,
    redraw,
    sync-panes,
    next-tab,
    prev-tab,
    copy-pane,
    toggle-key-lock,
    scroll-up,
    scroll-down,
    lazygit,
    yazi,
    editor,
    open-in-ide,
    show-diff,
    save-layout,
    apply-layout,
    export-layout,
    import-layout,
    detach,
    quit,
    quit-kill,
    focus-left,
    focus-right,
    focus-up,
    focus-down,
    resize-left,
    resize-right,
    resize-up,
    resize-down,
    swap-pane-left,
    swap-pane-right,
    swap-pane-up,
    swap-pane-down,
    nav-left,
    nav-right,
    nav-up,
    nav-down,
  ]
---

# Terminal & panes

The center is a real terminal multiplexer: each worktree tab holds a tree
of PTY panes.

## Tabs

- `Alt-t` — new tab on the _same_ worktree; `Alt-T` — a standalone
  terminal tab (no worktree).
- `Alt-←/→` — move to the pane on the left / right (see Splits); at the pane
  edge, with no pane left in that direction, it switches to the previous / next
  tab. It never focuses the [[sidebar]] / [[panel]] — that is `Ctrl-←/→`.

Falling off the pane edge is the usual way to change tabs, so `next-tab`
and `prev-tab` carry no chord of their own. Bind them in `[keybinds]` if
you want a direct key.

## Splits

- `Alt-p` — smart split (along the pane's longer dimension)
- `Alt-n` / `Alt-N` — split down / split right
- `Ctrl-←/↓/↑/→` (or `h/j/k/l`) — move focus between panes and out to the
  [[sidebar]] / [[panel]]
- `Alt-←/→` — move to the pane on the left / right, but a move that runs off
  the pane edge falls through to a previous / next tab switch, so one key walks
  the row of panes and keeps going into tabs. Unlike `Ctrl-←/→` it never steps
  into the [[sidebar]] / [[panel]].
- `Alt-↑/↓` — move to the pane above / below; at the top / bottom pane it
  switches to the previous / next worktree within the current workspace. This
  never focuses the top / bottom bars — that is `Ctrl-↑/↓`.
- `Ctrl-Alt-z` — zoom the focused pane; cycles tiled → maximized → full-window
- `Ctrl-Alt-y` — sync panes: broadcast typed input to every pane in the tab

## Launch menu

`Ctrl-Alt-l` (or the **Launch menu** command in the [[command-palette]]) opens a
picker that launches something into the active worktree: your `[[presets]]`
first (each with its description), then the same agents, tools, and `shell` the
new-worktree wizard offers.

- Picking an **agent** opens a new tab running it and remembers it as the
  worktree's agent (so a restart and the activity dots follow the launch).
- Picking a **tool** or **shell** opens a new tab; the remembered agent is left
  alone.
- Picking a **preset** applies its whole shape: `mode = "split"` opens all its
  commands as an even split in one new tab, `mode = "tabs"` opens one tab per
  command. Each command resolves first as an `[[agents]]`/`[[tools]]` name (with
  its command, sandbox, and provider), otherwise runs via the login shell. A
  preset never changes the remembered agent — it only launches panes.

Presets are declared in `[[presets]]` (name, description, `commands`, `mode`,
worktree-relative `cwd`, an `env` overlay, and an optional saved-`layout` ref);
put secrets behind `env:`/`file:` refs, never raw. A `[[worktree_templates]]`
entry can carry `preset = "<name>"` to open with that shape at creation, and
`thegn open <repo> --preset <name>` applies one on arrival (see [[cli]]).

### Resize & move

- `Ctrl-Shift-←/↓/↑/→` — **resize** the focused pane one step toward that side,
  growing it and shrinking the neighbour it shares that border with. It stops
  before a pane collapses to nothing, and a direction with no neighbour to give
  up room says so in the statusbar rather than doing anything.
- `Alt-Shift-h/j/k/l` — **swap** the focused pane with the neighbour in that
  direction (the same neighbour `Ctrl`-focus would land on). The two panes trade
  places, each adopting the other's slot size, and focus follows the pane you
  moved. A whole stack moves as one unit. Both resize and swap survive detach —
  the new weights persist with the tab layout.
- **Mouse on a pane's frame** — clicking a pane's title bar or outer edge
  focuses that pane (without typing into it), the same click a drag starts
  from: press on the frame and move to **rearrange** — drop onto a pane to
  swap with it, onto an edge to anchor beside it. `Esc` abandons the drag and
  a release without motion never moves anything.

## Tools, scoped to the focused worktree

- `Alt-g` lazygit · `Alt-e` `$EDITOR` · `Alt-/` git diff
- `Alt-y` / `Ctrl-Alt-f` — the bottom files drawer (see [[drawer-and-corner]])

**Open focused worktree in IDE** (`open-in-ide`) is a separate palette action:
it hands the whole worktree to the provider selected by `[editor] provider` (and
the trusted `[workspace.<slug>] editor` override). Windowed providers launch
detached; terminal providers open in a thegn tab. It has no default chord, so
use the [[command-palette]] or bind the action id yourself. The `editor` action
and `Alt-e` remain the terminal editor tool.

## Copy, search, replay

- `Ctrl-Alt-c` (or `Ctrl-Shift-c`) — copy the selection, or the whole
  visible pane when nothing is selected. Mouse drags select and copy on
  release.
- `Ctrl-Alt-/` — search the focused pane's history; `Ctrl-/` searches
  across panes.
- `Alt-r` — time-travel replay of the focused pane (needs `[replay]`
  enabled); `Ctrl-Alt-r` toggles the session recorder.
- Inside replay, `e` (or the **export-cast** palette action) writes the
  retained history to an asciicast `.cast` file under the recordings dir and
  reports the path and the timespan it covers. It is honestly a tail — only what
  the `[replay]` budget still holds — and fails with a clear message when replay
  is disabled or the pane has nothing recorded.

[[copy-and-select]] covers all of this properly, including scrollback,
registers, and what happens over ssh.

## When keys collide

`Ctrl-g` locks the keymap: every chord passes through to the pane until
pressed again. Use it when a TUI inside the pane needs chords thegn owns.

## Fixing a garbled screen

`Ctrl-Shift-l` forces a full redraw — the classic Ctrl-L "fix my screen"
escape hatch. Reach for it if the display drifts (repeated or leftover
lines) after the outer terminal window regains focus: some terminals
repaint their alt screen imperfectly and thegn gets no event to heal it
automatically.

## Layouts

Named layouts snapshot a tab's pane tree: save, apply, export, and import
them from the [[command-palette]].

## Leaving

- **detach** — leave everything running and drop back to your shell.
- **quit** — close the UI; daemon-backed panes keep running.
- **Quit and kill sessions** (`quit-kill`) — the explicit "I'm done":
  ends the sessions too.

Only the first two are things you reach for daily; `quit-kill` has no
default chord and lives in the palette. [[daemon-and-sessions]] explains
what survives which.
