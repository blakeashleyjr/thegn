---
id: system-monitor
title: System monitor
parent: bars
order: 1
actions: [open-monitor]
---

# System monitor

A tabbed, live view of what this machine is doing: CPU, memory, temperature,
network, disk, GPU, power, and processes. `Ctrl-Alt-M` opens it, or run
**System monitor** from the command palette.

You can also get here from the masthead. Focus the top bar with `Ctrl-↑`, walk
to a stat chip with `←`/`→`, and press `↵` — that opens the chip's own popup;
press `↵` again (or `M`) to expand it into the full monitor at the matching tab.
Chips that can expand say so in their top-right corner.

## Tabs

`Tab` and `Shift-Tab` walk the tabs; so do `←`/`→` and `h`/`l`. The number keys
`1`–`9` jump straight to one.

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

- `[` and `]` narrow and widen the **time window**: 30s, 2m, 10m, 1h, all. When
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

## Processes

The Processes tab lists the heaviest processes by CPU and by memory, with
thegn's own panes and its pane daemon called out in the `owner` column.

- `c`, `m`, `n`, `p` sort by CPU, memory, name, or PID.
- `r` reverses the sort direction.

Enumerating every process is the one genuinely expensive reading thegn takes, so
it only happens while this tab is open and unpaused — closing the tab stops it
entirely. The first sample after you open the tab shows `—` for CPU, because CPU
is measured as a change between two readings and there is not yet a previous one.

Processes inside a `podman` or `docker` pane are attributed to the sandbox rather
than to the pane: the real work runs in a different PID namespace, where the host
cannot see it. `bwrap` panes attribute normally.

Set `[monitor] processes = false` to disable the tab entirely, and
`[monitor] proc_rows` to change how many rows it shows.

## Containers

The Containers tab lists every container on the machine across the detected
backends, thegn's own first (tinted, foreign ones ghosted), with each one's
status/health and — while the tab is open — live CPU, memory and network. The
header sums thegn's footprint: how many owned containers, images and volumes,
and the engine's on-disk total (marked `≥` when a backend can't report disk
usage). The tab only appears when a container engine is present.

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
