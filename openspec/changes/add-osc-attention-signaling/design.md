# Design

## The parse (host, emulator seam)

The `PaneEmulator` already consumes PTY bytes through the vt/alacritty parser.
OSC dispatch is surfaced (or intercepted) at that seam:

- `OSC 9 ; <text> ST` → `AttentionSignal { kind: Notify, title: None, body: text }`
- `OSC 777 ; notify ; <title> ; <body> ST` → `AttentionSignal { kind: Notify,
title: Some(title), body }` (the `777` sub-command is `notify`; other
  sub-commands are ignored/passed through).

Parsing is a **pure function** in `superzej-core`
(`attention::parse_osc(params: &[&[u8]]) -> Option<AttentionSignal>`) so it is
unit-tested against the 95% core gate (well-formed `9`/`777`, missing body,
non-`notify` `777`, oversized payload truncation, non-UTF-8 → lossy). The host
emulator only forwards the OSC params to this parser and, on `Some`, emits an
event.

## The CLI verb (host)

`szhost notify [--title T] [--worktree PATH | --pane ID] <body>` resolves the
target worktree/pane (defaulting to `$SUPERZEJ_WORKTREE` / `$SUPERZEJ_PANE`
env, mirroring the per-pane env-firewall wiring already exported into panes) and
raises the _same_ `AttentionSignal` over the running host's control path. When no
host is running it exits non-zero with a clear message (it is a live-session
verb). This gives non-OSC processes and shell hooks a way in.

## Routing to the existing pipeline

The signal is mapped to an `EventBus` event (`Event::Attention { worktree,
pane, title, body }`) at the point of emission, then flows through the
**unchanged** consumer chain:

- The **activity state machine** (`superzej-core::activity`) gains an explicit
  `AttentionRequested` transition that sets the pane/worktree dot to the
  waiting/needs-attention state and makes it **sticky** (reusing the existing
  `RESUME_GRACE_SECS` sticky-state logic) until the process resumes output or the
  human focuses the pane — so a CPU blip can't clear an explicit hand-raise.
- The **notification** path derives a desktop/inbox notification exactly as other
  events do (respecting whatever routing/priority is configured).
- The **sidebar badge** and **statusbar** already render from these — no new
  chrome logic, just a repaint via the master `dirty` (chrome) channel.

## Attention ordering (sidebar sort)

The signal is not only a badge — it can drive **row order**. `sidebar.rs` already
has a `SortMode` enum (Manual/Name/Recent/Activity) and a `sort_groups()`
comparator whose Activity branch ranks `active > waiting > read > none`. This adds
a `SortMode::Attention` whose comparator ranks by _who needs the human_:
`urgent-flag > waiting(needs-human) > error > idle-ready > running`, with a
longest-waiting-first tie-break (using `last_active_at`/`busy_since` already on
the activity `Entry`). The urgent flag is a new carrier on `SidebarRow` (populated
from a `urgent_flags` map on `SidebarStatus`), set by the same
`Event::Attention` when it carries an urgent marker and cleared on resume/focus
like the sticky needs-attention state.

This is deliberately kept distinct from `add-notification-prioritization`, which
sets _flag color/urgency_ (Alert/Notice/Info) — that decides how loud a row is;
this decides _where a row sits_. The two compose: a row can be Alert-colored and,
under the attention sort, also floated to the top. The sort is **opt-in** (a
`SortMode` the user selects); default order is unchanged.

## Invariants

- **Event loop**: no new timer or polling. The signal is edge-triggered — it
  arrives on the PTY reader thread (OSC) or the control path (CLI), which already
  send on the mpsc channel **and** pulse the `TerminalWaker`. The loop drains on
  wake, as today. A sort-order change is a chrome `dirty` recompute on the next
  wake, not a new tick.
- **Render**: setting a dot / badge is a **chrome `dirty`** repaint, never a
  per-pane-output recompose. Pane content that merely _contained_ the OSC bytes
  is still a `Panes` diff. `render_plan` invariants unchanged.
- **State**: no `user_version` bump. The signal is ephemeral; any durable
  reflection is the existing notification inbox row.
- **Additivity**: the parser and event are process-agnostic and live in core; no
  dependency on the agent/proxy layers.

## Alternatives considered

- **Only screen-phrase matching (S 253)** — kept as the fallback, but it is
  fragile (localized/animated agent UIs) and can't fire for headless processes.
- **A bespoke superzej escape sequence** — rejected; `OSC 9`/`OSC 777` are
  already emitted by real tools and terminals, so adopting them means zero
  producer-side work for the ecosystem.
