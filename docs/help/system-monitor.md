---
id: system-monitor
title: System monitor
order: 8
actions: [open-monitor, open-pipeline-board]
contexts: [overlay:monitor]
---

# System monitor

A tabbed, live view of what this machine is doing: CPU, memory, temperature,
network, disk, GPU, power, and processes. `Ctrl-Alt-Shift-M` opens it (the
capital `M` in the binding is the Shift — `Ctrl-Alt-m` is the notification-mode
cycle), or run **System monitor** from the command palette. Pressing the same
chord again closes it, and `Ctrl-g` still toggles the key lock rather than
dismissing the monitor.

You can also get here from the masthead. Focus the top bar with `Ctrl-↑`, walk
to a stat chip with `←`/`→`, and press `↵` — that opens the chip's own popup;
press `↵` again (or `M`) to expand it into the full monitor at the matching tab.
Chips that can expand say so in their top-right corner.

## Tabs

`Tab` and `Shift-Tab` walk the tabs; so do `←`/`→` and `h`/`l`. Each tab in the
bar carries its own number, and typing that number jumps straight to it —
`1`–`9`, then `0` for the tenth, so the last tab is reachable too. The numbers
count the tabs you can actually see, so `2` means the same thing on a laptop and
on a GPU-less server. On a terminal too narrow for the whole strip, the bar
scrolls to keep the active tab whole and marks the tabs scrolled off each end,
rather than clipping the tab you are on.

Tabs only appear for hardware this machine actually has — no battery means no
Power tab, no discrete GPU means no GPU tab. A metric that disappears while
you are looking at it (an unplugged battery, an unmounted disk) moves you to a
tab that still has something to show.

On **Apple silicon**, temperatures come from the machine's HID thermal sensors
rather than the SMC keys that only exist on Intel Macs — so the temperature tab
works there, showing the per-die sensors plus storage and battery. If a future
macOS stops exposing them, the tab hides rather than showing zeroes.

## Reading the graphs

Every plot has "now" at its right edge. Three toggles change how the history is
drawn, and each one is remembered **per tab** — so CPU can sit on a ten-minute
log scale while Network stays on a live linear one.

- `[` and `]` narrow and widen the **time window**, stepping along the rungs of
  `[monitor] window_ladder` — 30 seconds up to `all` out of the box, starting at
  `[monitor] default_window`. The ladder is configurable, so the footer always
  names the rung you are actually on rather than a list from the manual. When
  the window is wider than the history collected so far, the header says so
  (`1h · 4m of history`) rather than implying an hour of flat readings.
- `g` cycles the **graph style**: filled area, line, or a single-row sparkline.
  Sparkline mode collapses each block to one row, which is how a machine with a
  dozen thermal sensors fits on one screen.
- `s` cycles the **scale**. `window` scales against the tallest value in view,
  which shows shape but hides magnitude; `fixed` uses the metric's real full
  scale, so a quiet signal reads honestly quiet; `log` spreads out a rate that
  spans orders of magnitude. Metrics with no natural ceiling — network rates,
  load average — fall back to `window` under `fixed`, since there is no honest
  number to divide by.

`Space` **pauses**. That freezes the picture, not the recording: samples keep
accumulating underneath, so resuming shows a continuous timeline with no gap.
Pausing also drops the sampler back to its normal rate, so a frozen monitor is
cheaper than a live one.

Note two deliberate differences from the other overlays: `Space` is pause here
(use `PgDn` to page), and `g` cycles the graph style rather than jumping to the
top (use `Home` or `G`).

`j`/`k` and the arrows scroll; `PgUp`/`PgDn` page; `Home` and `G` jump to the
ends. `Esc` or `q` closes, as does a click outside the box.

On the **list** tabs — Processes, Disk, Containers and Pipeline — those same
keys move a **row cursor** rather than the viewport: the highlighted row moves
and the view follows it, so the row an action key acts on is always a row you
can see.

`?` (or `F1`) opens this page without leaving the monitor. The footer
advertises only the keys the tab in front of you actually has: the graph
toggles appear on the graph tabs, the row actions on the tab that owns them,
and `spc` pause on every tab — including the Pipeline board, which `Space`
freezes like any other.

## Processes

The Processes tab lists the heaviest processes by CPU and by memory, with
thegn's own panes and its pane daemon called out in the `owner` column.

- `c`, `m`, `n`, `p` sort by CPU, memory, name, or PID.
- `r` reverses the sort direction.
- `/` opens an incremental filter over process name, PID, and owner (type to
  narrow, `Backspace` to edit, `Enter` to apply, `Esc` to clear). `Esc` backs
  out of the filter first — it does not close the monitor while a filter is open.
- `t` toggles a tree view grouped by the sampled parent chain. Because the tab
  only keeps the heaviest processes, a child whose parent fell outside that set
  is hoisted to a top-level row and marked with a leading `…`, rather than
  hiding it or enumerating every process on the machine.
- `x` signals the selected process — the row under the cursor, which the arrows
  and `j`/`k` move and the view follows, so it is always a row on screen. It
  always asks first: the confirmation names
  the PID, process name, and owner, so a pane-owned build is recognizably
  thegn's own. The first `x` sends a graceful terminate (SIGTERM); pressing `x`
  again on the same process offers a hard kill (SIGKILL) as a separate,
  explicit step. A signal that fails — no such process, permission denied — is
  shown in the footer, never silently swallowed. This is a local, terminal-only
  action: it is not exposed to the CLI, the control API, or any remote surface.

Enumerating every process is the one genuinely expensive reading thegn takes, so
it only happens while this tab is open and unpaused — closing the tab stops it
entirely. The first sample after you open the tab shows `—` for CPU, because CPU
is measured as a change between two readings and there is not yet a previous one.

Processes inside a `podman` or `docker` pane are attributed to the sandbox rather
than to the pane: the real work runs in a different PID namespace, where the host
cannot see it. `bwrap` panes attribute normally.

Set `[monitor] processes = false` to disable the tab entirely, and
`[monitor] proc_rows` to change how many rows it shows.

## Disk

The Disk tab shows live disk I/O and per-volume free space at the top, then a
**worktree lane**: each worktree's total size and its reclaimable `target/`
share, biggest first, with how long ago each was measured. The sizes come from
the background `[disk]` scanner's cache — opening the tab never starts a `du`
walk, and a stale row shows its age rather than blocking on a rescan.

`x` cleans the selected worktree's `target/` (the manual sibling of
`[disk] auto_clean_on_merge`), after a confirmation. The checkout is kept; only
regenerable build artifacts go, off the event loop, and the row updates after
the next measurement. As on Processes, the arrows and `j`/`k` move the row
cursor and the lane scrolls to follow it, so `x` acts on the worktree you can
see highlighted rather than on one that scrolled away.

When the recorded free-space trend is clearly downward, the worktrees-filesystem
heading also shows a **time-to-full projection** ("filling · full in ~2d"). It
is deliberately conservative: with a flat or growing disk, or too little history
to extrapolate honestly, it shows nothing rather than a confident wrong number.
The optional `[stats.alerts] disk_eta` rule (off by default) fires off the same
projection when the runway drops below a configured number of hours.

## Containers

The Containers tab lists every container on the machine across the detected
backends, thegn's own first (tinted, foreign ones ghosted), with each one's
status/health and — while the tab is open — live CPU, memory and network. The
heading is just `containers`: the note beside it leads with the ownership
split — how many are thegn's own, how many are foreign — because the list
deliberately includes rows thegn did not create. On an engine that reports a
footprint it continues with images, volumes and the engine's on-disk total
(marked `≥` when a backend can't report disk usage); otherwise with how many
are running. The tab only appears when a container engine is present.

Sampling those per-container numbers (and the disk total) is the one expensive
reading here, so — like Processes — it runs **only while this tab is open**;
close it and the ambient refresh drops back to the cheap listing. The disk
total refreshes on a slower cadence than the stats.

Actions apply only to **thegn's own** containers (foreign ones are read-only):

- `↵` opens a shell inside the container in a new pane.
- `o` tails its logs live in a new pane.
- `t` stops it, `r` restarts it.
- `x` removes it — press `x` again to confirm; a **running** container asks you
  to confirm a force-remove.

Stop, restart and remove run off the UI loop and report their outcome as a
toast; the row's own status catches up on the next refresh. Managing containers
thegn did not create is deliberately not offered — run `lazydocker`/`oxker` in a
pane for that. Estate cleanup from the command line is `thegn sandbox gc` /
`thegn sandbox prune` (see [[sandboxing]]).

## Pipeline

`Alt-b` opens the monitor straight on this tab, from anywhere — or run
**Pipeline board** from the command palette. It is the board's own direct door;
from inside the monitor the board's tab number reaches it like any other.
Pressing `Alt-b` again closes the monitor; pressing it while the monitor sits on
another tab jumps to the board instead.

While any agent dispatch is live, the sidebar also grows a **Pipeline** row just
above the `TERMINALS` banner — `Pipeline ▸ 3 running`, plus a waiting count when
a stage is parked on a human. `↵` or a click on it opens the board.

The Pipeline tab is the agent-dispatch roster, grouped by stage: one block per
pipeline stage, with each row's status, the agent running it, the worktree it
works in, the issue it came from, and how long it has been going. Work chunked
out of another row — an architect fanning out to coders — renders indented under
its parent. Dispatches made outside a pipeline group last, under `unstaged`.

Every stage you have configured appears, in configured order, whether or not
anything is running in it — an idle stage shows its heading and says so instead
of vanishing, so the board reads as the whole org chart rather than only its
busy corner. Each configured stage's heading also carries its agent, its
concurrency cap, and the stage work hands off to next, beside the live count.

The Lead that drives a chart is an agent running the `/pipeline` skill, and a
fleet over a batch of issues is `/supervise`: both are bundled in the binary
and seeded into every worktree's `.claude/skills/` (`/pipeline` once a chart is
configured, `/supervise` always, `/mq` when the merge queue is on), so an agent
in any project thegn opens finds them without installing anything. Stage
workers are launched with `thegn session open --agent <entry> --stage <stage>`,
which applies the stage's `model` / `env` / `permissions` overrides (see
[[configuration]]).

The tab appears only once something has been dispatched, or a pipeline is
configured. Like Processes and Containers, it re-reads the roster **only while
it is open**; closing it stops that entirely, and a change made elsewhere (a
finished agent pane, a dispatch recorded by a supervising agent) still reaches
it. `Space` freezes the board like any other tab — and, because the re-read is
the board's only refresh, a paused board is a _stopped_ board rather than a
slow one. The footer says `resume` while it is frozen.

`↵` on a row goes to that dispatch's worktree and closes the monitor. When the
worktree is already a tab here, that is the same jump as pressing `↵` on its
sidebar row. When it is not — a dispatch made by a supervising agent onto a
worktree this session never opened — `↵` now **opens** it as a tab rather than
reporting that it isn't open. The footer notice remains only for a worktree that
is genuinely gone: deleted under the board, or never registered.

The board is a **view**, not a controller: nothing here starts, advances or
stops a stage. Stage transitions belong to whatever is supervising the run,
which records them on the roster; thegn stores and shows them. Worktrees with a
live staged dispatch also carry a short stage tag beside their activity dot in
the sidebar, and a stage parked on a human reads as "needs you" there like any
other blocked agent.

## Alerts

Separately from this modal, thegn can warn you when a metric crosses a
threshold — see `[stats.alerts]` in the config reference. A reading has to stay
past the line for `sustain_secs` before anything fires, a standing alert repeats
at most every `repeat_secs`, and clearing needs the value to retreat past the
threshold by `clear_margin`, so nothing flaps. Alerts show as a toast; set
`notify = true` to also record them to the notification inbox.

## Configuration

`[monitor]` sets the starting `default_window`, `default_style` and
`default_scale`. They are only starting points — whatever you toggle is
remembered per tab from then on.
