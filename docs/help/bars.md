---
id: bars
title: Masthead & status bar
order: 7
contexts: [zone:masthead, zone:statusbar]
actions:
  [
    toggle-notifications,
    notify-dnd-toggle,
    notify-mode-cycle,
    attention-next,
    mark-all-read,
    open-ci,
    open-usage,
  ]
---

# Masthead & status bar

The top and bottom chrome bars. Both are focusable zones: `Ctrl-↑` from
the top pane row reaches the masthead, `Ctrl-↓` from the bottom reaches
the status bar (with the files drawer open, `Ctrl-↓` reaches the drawer
first); `Esc` returns to the center.

## Masthead (top)

The brand block and the app-tab chips on the left (switched from the
keyboard — the chips themselves are not clickable), and the
**stats cluster** on the right: the `[bars] top_right` widgets — by default
`cpu`, `mem`, `disk`, `gpu`, `temp`, `net`, `battery`, `weather`, `date`,
`clock`. With
the bar focused, `←/→` walks the stats and `↵` opens the stat's history popup
— `Esc`, `q`, or a click outside dismisses it. Clicking the empty right half
of the bar cycles the `[stats] refresh_rates` cadence. The cluster sheds
stats softest-first as the terminal narrows, `date` first, then `weather`,
and `clock` among the last to go — so the
calendar stays reachable from the bar even on a cramped terminal; the brand
and the active app chip always keep their cells.

Activating the date or the clock opens the **calendar** — a month grid, the
day's agenda, and your world clocks. `Alt-d` opens it from anywhere. See
[[calendar]].

The `weather` widget shows one condition glyph and the current temperature
(`☀ 18°C`), and it is **off by default**: it draws nothing at all until you
set `[weather] enabled = true`, the same way `gpu` is inert on a machine
without one. Enabling it is the consent step — that is when thegn first
contacts a weather provider. Clicking it opens the same calendar popup, where
a `WEATHER` block carries the full reading. A reading past
`[weather] stale_after_secs` dims rather than colouring — staleness is a
caveat, not an alert — and past `hard_expiry_secs` the widget disappears
entirely rather than show yesterday's sky. See
[the calendar page](calendar.md) for the popup block.

Their format strings are `[bars] date_format` and `clock_format` (chrono
strftime). Both are checked when config loads, so a typo warns and falls back to
the default instead of failing mid-render. The clock wakes only on minute
boundaries unless a format actually renders seconds (`%S`, `%T`, `%r`, `%X`), in
which case it ticks once a second.

## Status bar (bottom)

- **Left:** the `?` help chip (click for the context-sensitive page `F1`
  would give), the mode chip, and the contextual key hints — the keys that
  work _right now_, for whatever owns focus. The hints follow you: sidebar
  keys while the sidebar is focused, section keys while the panel is, and
  in the git-family sections only the keys that dispatch at the panel's
  current width (`e widen` stands in for the action keys at the resting
  width).
- **Right:** the `[bars] bottom_right` widgets (`pr`, `tests`, `loc`,
  `disk`, and `status` — the transient status message, which clears itself
  after a few seconds), then the badge cluster: the do-not-disturb / routing
  mode chip, the attention `✋` chip (or the quiet `✉` inbox count),
  `offline`, CI, PR-queue, disk-warn, share `⇅`, media `▶`, zoom / max /
  `LOCKED` / `SYNC`. The merge queue is not in the default bar; add `"mq"`
  to `bottom_right` for a compact scoped summary.
- **Far right:** the always-on **daemon/status indicator** — a single glyph,
  no label. It is the one badge that is never silent, since it is a persistent
  affordance rather than an alert.

When the bar is too narrow for everything, the free-text status message is
clipped first, then widgets and the quieter badges are shed — the `✋`,
`LOCKED` and daemon chips are never the ones to go.

The `loc` and `disk` widgets are **measured in the background**, not computed
per frame: a `du` and a `tokei` walk are both far too slow to sit on the render
path. A worktree you have never opened gets both within a second or two of being
created or switched to, and they refresh on the `[loc]` / `[disk]`
`scan_interval_secs` cadence after that — plus, for `loc`, whenever you actually
edit the worktree you are in (`[loc] watch_invalidate_secs`). A chip that is
absent means "not
measured" — remote and provider worktrees are deliberately never measured on the
host (their checkout lives in the env, so the host path would be a stub), and
neither widget ever prints a placeholder zero. Turn either off with
`[loc] enabled = false` or `[disk] show_sizes = false`, which also hides
whatever was already measured.

The hint strip is the quick reference; this help (`F1`) and the
[[keybindings]] page are the complete one.

## Daemon / status indicator

Pinned to the far right of the status bar, one glyph reports how this
instance relates to the pane daemon:

- `○` **non-persistent** — the focused pane runs inline; quitting ends it.
- `◆` **persistent** — the focused pane is daemon-backed: quitting detaches
  it (the process keeps running) and the next launch reattaches.
- `▲` **server** — this instance's daemon is serving remote thin clients.
- `▽` **client** — attached to a pane daemon on another machine.

(On ASCII terminals these degrade to `o`, `*`, `^`, `v`.) Activating it — a
click, or `↵` with the bar focused — opens the **status modal**, a scrollable
dashboard (`j`/`k` or the arrow keys, `esc` to close):

- **daemon** — role, PID, version, host, uptime, heartbeat age (`healthy` while
  the daemon is still discoverable, `stale` once its heartbeat lapses), registry
  id, transport, endpoint and scope paths, serve address, and the live
  `[daemon]` policy: how long a detached pane stays warm (`lease_grace_secs`),
  when an idle daemon exits (`idle_exit_secs`), whether new panes route through
  it at all, and how many of the panes on screen right now would survive a quit.
- **sessions** — every session the daemon owns: id, program, worktree, size,
  attached clients, age, and the warm-lease countdown for detached ones. Read
  live from the daemon when you open the modal and re-probed every few
  seconds while it stays open; a list that could not be refreshed is marked
  `as of N ago` in amber, and a daemon that stops answering drops the chip
  back to `○` straight away.
- **thegn process** — resident memory (RSS), this process's over the pane
  daemon's in one stacked plot, plus the daemon's CPU and memory trends.
- **loop** — the event-loop rollup: wakes/s, render / input / flush / drain /
  switch latency, the frame mix, PTY throughput, and the idle and render-busy
  ratios. Populated only with `THEGN_PERF=1` (or while the Telemetry panel
  section is open); otherwise it says so.

The modal narrows gracefully: below ~70 columns the two-column key/value grids
fold to one and the session table sheds its worktree and size columns.

## Notifications & attention

The badge cluster carries its own actions (palette-runnable, all
bindable): `Ctrl-Alt-i` toggles the notifications panel section
(`toggle-notifications`), `Ctrl-Alt-d` toggles do-not-disturb
(`notify-dnd-toggle`), `Ctrl-Alt-m` cycles the routing mode
(`notify-mode-cycle` — modes come from `[notifications.modes]`; with none
configured it reports `default`), `Alt-Shift-R` marks everything read
(`mark-all-read`), and `Alt-a` jumps to the next item that needs you
(`attention-next`). While do-not-disturb or a non-default mode is on, a
`● dnd` / `◉ <mode>` chip appears at the head of the badge cluster.
Activating the CI badge opens a **CI runs** popup (`↵` view, `o` open, `r`
rerun, `c` cancel); the palette's `open-ci` action opens the Work ▸ CI
panel section instead.

There is **one attention chip**. `✋ N` counts what needs you: worktrees at
a needs-you tier (blocked on input, failed, finished-awaiting-review) plus
unread alert-priority notifications not already covered by one of those
worktrees — red while anything is blocked/failing or an alert is unread,
amber when only finished work waits. When nothing needs you but
notice-priority rows are unread, a quiet blue `✉ N` takes its place;
info-priority rows never show on the bar. (Older builds had a second `⚑`
inbox flag beside the hand; one failed pane lit both.)

An agent's own "I need you" — the `OSC 9` / `OSC 777` signal a coding CLI emits
when it hands the turn back — is **live state, not an inbox entry**: it shows as
the sidebar dot and the `✋` chip and clears the moment you answer. It reaches
the notification list only if you turn on
`[notifications] agent_attention_inbox`.

The `✋`/`✉` chip, the opt-in `mq` widget, and the PR-queue chip all open the
**one unified surface** — a single grouped list of _Needs you · Alerts · Merge
queue · Notifications · Other repos · Logs_, sized to the terminal and
showing **only unread** rows (read history lives in the panel's System ▸
Notifications section behind its show-read toggle). Moving the cursor never
marks anything read. One clear/dismiss convention holds throughout: `x`
dismisses the row under the cursor and it leaves the list in place, the
popup staying open (on a needs-you row that quiets the worktree **and**
retires its inbox rows); `a` clears all — the same total clear from every
row — and closes the popup. Merge-queue rows act in place — `l` lands a gated-green branch, `r`
retries a blocked one, `x` removes it, `m` jumps to the full Work ▸ Merge
queue section. (In the panel's System ▸ Notifications section the keys are
`x` mark read, `d` dismiss, `a` clear all — and `a` covers this repo's rows
plus the host-global ones, counting a row tagged to the repo's own main
checkout as this repo's, which it always displayed but never used to clear;
`A`/`g` widens to every worktree.) Ephemeral confirmations
("Landed") surface as transient toasts — the passing view of routed events
that also land in this inbox — and direct acknowledgements ("Text copied")
are toasts only.

### Scope: this repo by default

The `✋` chip, the "Needs you" list, and `Alt a` (jump to next) are **scoped
to the active worktree's repo**, like the notification inbox — a sibling
repo's failing CI shouldn't nag you in the repo you're working in. Nothing is
hidden: worktrees elsewhere are counted as a dim `+N` beside the chip and
listed under **Other repos**, still one `↵` away (which switches workspace if
the worktree's tab isn't open). A terminal tab scopes to its session's repo.
Press `g` in the panel's System ▸ Notifications section to widen every scoped
view to all worktrees, and again to narrow back.

### Clearing, and what "clear" covers

`a` — in the inbox, in the unified surface (from any row), and as
`Alt-Shift-R` — is the clear. It marks the notifications the current scope shows read (this
repo's + host-global rows by default; every repo's only under the `g`
all-worktrees view) **and** acknowledges the live needs-you signals,
including the worktree you're currently on. That second
half matters: "CI failed", "PR has conflicts" and "changes requested" are
derived from the PR/CI caches rather than from an inbox row, so marking
notifications read alone would leave the `✋` chip lit and it would reappear
on the next refresh. Use `x` on a "Needs you" row to quiet just that one.

An acknowledgement is bound to the **episode** it was made against — the CI
run, or the PR's head commit. So it survives restarts, refreshes, and being
temporarily outranked by something more urgent; but a new run, or a new push,
is a new episode and does raise the signal again.

## AI account usage

The `◔` gauge shows the most-consumed rate-limit window across your AI coding
accounts, with its reset countdown — green, amber past `[usage] warn_percent`,
red past `crit_percent`. Unlike the badges above it is not silent when healthy: it
reports a level, not an exception. Activating it — or `Alt-u`, the `open-usage`
action — opens the per-account overlay.

See [AI account usage](ai-usage.md) for tracking several accounts, where the
numbers come from, and the `[usage.alerts]` warning thresholds.
