---
id: system-monitor
title: System monitor
order: 8
actions: [open-monitor, open-pipeline-board]
contexts: [overlay:monitor]
---

# System monitor

A tabbed, live view of what this machine is doing: Overview, CPU, memory,
temperature, network, disk, GPU, power, processes, containers, and notifications.
`Ctrl-Alt-Shift-M` opens it (the capital `M` in the binding is the Shift —
`Ctrl-Alt-m` is the notification-mode cycle), or run **System monitor** from
the command palette. Pressing the same chord again closes it. `Ctrl-g` still
toggles the key lock rather than dismissing the monitor.

You can also get here from the masthead. Focus the top bar with `Ctrl-↑`, walk
to a stat chip with `←`/`→`, and press `↵` — that opens the chip's own popup;
press `↵` again (or `M`) to expand it into the full monitor at the matching tab.
Chips that can expand say so in their top-right corner.

## Tabs

`Tab` and `Shift-Tab` walk the tabs; so do `←`/`→` and `h`/`l`. Each of the
first ten tabs carries its number, and typing that number jumps straight to it —
`1`–`9`, then `0` for the tenth. If every optional tab appears, Notifications
is eleventh and has no digit key.
The numbers count the tabs you can actually see, so `2` means the same thing on
a laptop and on a GPU-less server. On a terminal too narrow for the whole
strip, the bar scrolls to keep the active tab whole and marks the tabs scrolled
off each end, rather than clipping the tab you are on.

Tabs only appear when their source is available — no battery means no Power
tab, and the GPU tab appears when a GPU reports utilisation. AMD cards and APUs
use sysfs, NVIDIA uses NVML when its library is installed and falls back to
`nvidia-smi`, and Apple silicon uses its system GPU reader. A metric that
disappears while you are looking at it (an unplugged battery, an unmounted
disk) moves you to a tab that still has something to show.

On **Apple silicon**, temperatures come from the machine's HID thermal sensors
rather than the SMC keys that only exist on Intel Macs — so the temperature tab
works there, showing the per-die sensors plus storage and battery. If a future
macOS stops exposing them, the tab hides rather than showing zeroes.

When a tab has several panels, `f` and `F` focus the next and previous panel,
and `z` zooms or unzooms the focused panel. In normal monitor navigation, `Esc`
leaves zoom before closing the monitor. The footer shows the `f panel` and
`z zoom` hints only when more than one panel is placed; the tab bar shows
`+N hidden` when panels do not fit. On a tab with a single panel, these keys do
nothing, and the footer leaves them out.

## Reading the graphs

Every plot has "now" at its right edge. Three toggles change how the history is
drawn, and each one is remembered **per tab** — so CPU can sit on a ten-minute
log scale while Network stays on a live linear one.

- `[` and `]` narrow and widen the **time window**, stepping along the rungs of
  `[monitor] window_ladder` — 30 seconds up to 24 hours out of the box, starting
  at `[monitor] default_window`. A custom ladder may end in `all`, meaning
  everything collected. The ladder is configurable, so the footer always names
  the rung you are actually on rather than a list from the manual. The monitor
  currently keeps about an hour of history; wider rungs show the available
  history, which the header reports (`1h · 4m of history`) rather than implying
  an hour of flat readings.
- `g` cycles the **graph style**: filled area, line, or a single-row sparkline.
  Sparkline mode collapses each block to one row, which is how a machine with a
  dozen thermal sensors fits on one screen.
- `s` cycles the **scale**. `window` scales against the tallest value in view,
  which shows shape but hides magnitude; `fixed` uses the metric's real full
  scale, so a quiet signal reads honestly quiet; `log` spreads out a rate that
  spans orders of magnitude. Metrics with no natural ceiling — network rates,
  load average — fall back to `window` under `fixed`, since there is no honest
  number to divide by.

`Space` **pauses**. That freezes the picture. The history graphs keep recording
at the normal, non-live rate, so resuming shows no gap; the monitor's detailed
sampler stops until you resume. That makes a paused monitor cheaper than a live
one.

On the Overview, CPU, Memory, Thermal, Network and Power tabs, and on the GPU
tab when an amdgpu device is present, the sampler reads only what the panels
on screen need. The other tabs, and the GPU tab on other hardware (where it
keeps the NVML-backed sections), still read every collector.
When paused or covered, the monitor's detailed sampler parks; nothing runs once
the monitor is closed.

Note two deliberate differences from the other overlays: `Space` is pause here
(use `PgDn` to page), and `g` cycles the graph style rather than jumping to the
top (use `Home` or `G`).

`j`/`k` and the arrows scroll; `PgUp`/`PgDn` page; `Home` and `G` jump to the
ends. `Esc` or `q` closes, as does a click outside the box.

On the **list** tabs — Processes, Disk and Containers — those same
keys move a **row cursor** rather than the viewport: the highlighted row moves
and the view follows it, so the row an action key acts on is always a row you
can see.

`?` (or `F1`) opens this page without leaving the monitor. The footer
advertises only the keys the tab in front of you actually has: the graph
toggles appear on the graph tabs, the row actions on the tab that owns them,
and `spc` pause on every tab.

## Overview

Overview is the default landing tab. It summarizes CPU and per-core activity,
load and pressure, memory health, network rates, disk activity, thermal state,
power when a battery is present, and process/container counts. Panels appear
when their source has a reading; absent readings use the terminal absent mark.

## CPU

The CPU board shows total busy history and the current user, system, iowait, and
steal shares, plus a per-core busy heatmap. The `Cores (L3)` panel labels known
performance/efficiency cores with P/E badges and adds an `L3<n>` tag (such as
`L30`) when the cache group is known; missing topology stays unlabelled. The
`C states residency` panel shows C0 and reported C-state residency per core.
Frequency, load/pressure, IRQ/softirq, and scheduler wait have separate panels.
Panel focus (`f`/`F`), zoom (`z`), and Escape follow the monitor board controls.

## Memory

The Memory tab is a panel board. `memory.composition` shows total, available,
used, free and the component breakdown with bounded usage history from the
selected telemetry window, scaled against total memory; frame samples and
telemetry history use separate rings. `memory.verdict` summarizes memory health;
`memory.pressure` shows PSI some/full pressure; `memory.swap` appears when swap
exists and includes selected-window history scaled against total swap when
samples are available;
`memory.paging` shows reclaim, refault and fault rates; and `memory.limit` shows
the process cgroup limit and thegn.slice usage. A missing limit is shown as
`—`; when every cgroup level is unlimited, the panel says `no limit`. Use
`f`/`F` to focus or unfocus a panel,
`z` to zoom it, and `Esc` to step back through focus and zoom. The memory and
swap masthead chips expand into this tab.

## Thermal

The Thermal tab shows the CPU package trend, per-core temperatures, the sensor
table with bounded per-sensor history, GPU temperatures and telemetry, thermal
zones with trips, and a per-core history heatmap. Missing readings use the
absent mark; no reading is displayed as zero. Panel demand follows the panels
placed on screen: `thermal.package`, `thermal.cores`, `thermal.sensors`,
`thermal.zones`, `thermal.gpu`, and `thermal.heatmap`. Use `f`/`F` to move panel
focus, `z` to zoom the focused panel, and `Esc` to unzoom before closing the
monitor. The temperature masthead chip expands into this tab.

## Network

The Network board has separate panels for mirrored receive/transmit throughput,
interfaces, and error/drop health. The throughput plot uses telemetry history;
the interface sparklines use the sampled per-interface rings. Totals count
physical links only, so VPN traffic is not double-counted. Interface rows keep
idle and down links visible, and fold interfaces beyond the eight history slots
into `other`. Use `f`/`F` to focus a panel, `z` to zoom it, and `Esc` to step
back through focus and zoom. The network masthead chip expands into this tab.

## Disk

The Disk tab shows live disk I/O and per-volume free space at the top. When
several mount points belong to one device, `+N` means N additional mount points
are folded into that row; I/O is counted once per physical disk. Below is a
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
projection when the runway drops below a configured number of hours. The Disk
masthead chip expands into this tab.

The `worktrees filesystem` block reports total, available, and used space.
On Linux, `thegn doctor` also reports Disk health from UDisks2 when available,
including health, critical warnings, wear, spare, read/write totals, power-on
time, and the last four serial characters. Missing serial or health values
show `n/a`. A partial report still lists the drives it could read and gives the
reason; if UDisks2 is unavailable, it says `n/a (udisks2 unavailable)` and
gives the reason. On other platforms the reason is `unsupported on this
platform`. The check does not prompt.

## GPU

On AMDGPU, the GPU tab has device, engine, memory, and clocks panels. Engine
busy history, VRAM/GTT use, clock level, and power against its reported cap are
plotted on their labelled 0–100% scales when readings are available. A runtime-
suspended card is not woken to sample it; its values are shown as unavailable
rather than zero. On APUs, shared memory is labelled `shared vram`.

On NVIDIA and Intel, the tab keeps the legacy GPU readings because this board's
engine counters are AMDGPU-only. The GPU masthead chip expands into the tab. When
several cards are available, it shows the busiest awake card.

On NVIDIA, the masthead uses NVML in process when `libnvidia-ml.so.1` is
installed; otherwise it reads through `nvidia-smi`.

## Power

The Power tab has three panels: `power.rails` plots socket and powercap history
against its labelled ceiling; `power.battery` plots charge against 100%, energy
against battery capacity, and draw magnitude against its labelled peak with the
current signed direction shown; and `power.policy` shows profile, boost, and EPP
as status chips. Frame and telemetry histories use separate scales. Unavailable
readings use the platform's absent or warming mark.
Panel focus and zoom use `f`/`F` and `z`. The tab appears when a battery source
is available; the battery masthead chip opens it.

## Processes

The Processes tab lists the heaviest processes by CPU and by memory, with
thegn's own panes and its pane daemon called out in the `owner` column.

The table fits its default columns to the available width. Today it shows PID,
owner, CPU%, the CPU bar, RSS, and the sampled command name; tree indentation
stays inside the command column. The command is the sampled process name, not
its argument list.

At 120 columns and wider, a summary above the table shows task, thread and
kernel-thread totals; R/S/D/Z/T/I state counts; new and exited process counts
since the previous pass; and a CPU/RSS breakdown for the UI, daemon, panes and
other processes. The pane row also shows its process count. The D count uses
the warning tone when any process is in disk wait. The first pass leaves CPU
and new/exited values unknown until their baselines are available.

- `c`, `m`, `n`, `p` sort by CPU, memory, name, or PID.
- `r` reverses the sort direction.
- `/` opens an incremental, space-separated AND filter (type to narrow,
  `Backspace` to edit, `Enter` to apply, `Esc` to clear). A bare word matches
  process name, PID, or owner. Field terms include `user:blake`, `wt:feat`,
  `cpu>10`, `mem>1G`, `age<5m`, `pid:42`, and `owner:thegn`; `!term` negates
  a term. `cpu`, `mem`, `age`, `pid`, and `owner` are the fields currently
  applied. `user`, `wt`, `pane`, `ctr`, `s`, `io`, and `thr` are recognized but
  currently ignored; ignored and malformed terms are listed in the header.
  `Esc` backs out of the filter first — it does not close the monitor while a
  filter is open.
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
  explicit step. A signal whose sampled process changed or left the displayed
  list is refused with “process changed or left the sampled list; select it
  again”. A signal that fails — no such process, permission denied — is shown
  in the footer, never silently swallowed. When process identity cannot be
  pinned, the signal is refused and the reason is shown in the footer. This is
  a local, terminal-only action: it is not exposed to the CLI, the control API,
  or any remote surface.

Enumerating every process is the one genuinely expensive reading thegn takes, so
it only happens while this tab is open and unpaused — closing the tab stops it
entirely. The first sample after you open the tab shows `—` for CPU, because
CPU is measured as a change between two readings and there is not yet a previous
one.

Processes inside a `podman` or `docker` pane are attributed to the sandbox
rather than to the pane: the real work runs in a different PID namespace, where
the host cannot see it. `bwrap` panes attribute normally.

Set `[monitor] processes = false` to disable the tab entirely, and
`[monitor] proc_rows` to change how many rows it shows.

## Containers

The Containers tab lists every container on the machine across the detected
backends, containers proven to belong to this profile first (tinted, foreign
ones ghosted), with each one's status/health and — while the tab is open — live
CPU, memory and network. The heading is just `containers`: the note beside it
leads with the ownership split — how many are owned under that rule, how many
are foreign — because the list deliberately includes foreign containers. On an
engine that reports a footprint it continues with images, volumes and the
engine's on-disk total (marked `≥` when a backend can't report disk usage);
otherwise with how many are running. The tab only appears when a container
engine is present.

Sampling those per-container numbers (and the disk total) is the one expensive
reading here, so — like Processes — it runs **only while this tab is open**;
close it and the ambient refresh drops back to the cheap listing. The disk
total refreshes on a slower cadence than the stats.

Actions apply only to containers proven to belong to this profile for a
known worktree on that worktree's launch backend (including recognized
`-tgvpn`/`-tgagent` companions). A user's `thegn-foo`, another profile's
container, or the same name in another Podman store is foreign and read-only.
A new container may appear foreign until the next refresh. Actions on owned
containers:

- `↵` opens a shell inside the container in a new pane.
- `o` tails its logs live in a new pane.
- `t` stops it, `r` restarts it.
- `x` removes it — press `x` again to confirm; a **running** container asks you
  to confirm a force-remove.

Stop, restart and remove run off the UI loop and report their outcome as a
toast; the row's own status catches up on the next refresh. Managing foreign
containers is deliberately not offered — run `lazydocker`/`oxker` in a pane for
that. Estate cleanup from the command line is `thegn sandbox gc` /
`thegn sandbox prune` (see [[sandboxing]]). No masthead chip expands into this
tab.

## Notifications

The Notifications tab reports configured notification sinks and their kind,
queue drops, rate drops, retries, sent messages, and dead letters. It appears
when a sink is configured or a notification counter has been reported. No
masthead chip expands into this tab.

## Pipeline

The agent-pipeline board used to be a tab here. It is now its own surface —
`Alt b`, or **Pipeline board** from the command palette. See
[[pipeline-board]].

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
