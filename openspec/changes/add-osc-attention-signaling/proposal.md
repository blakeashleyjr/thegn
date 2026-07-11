# Add OSC attention signaling (agents raise their hand)

## Summary

Give any process running in a pane — an agent, a build, a test run — an
**explicit, authoritative way to say "I need attention"**, instead of thegn
having to _infer_ it from CPU and screen-scraping heuristics. Two inbound
channels feed one existing pipeline:

1. **OSC escape sequences** parsed at the PTY emulator seam — `OSC 9 ; <text>`
   (desktop-notification convention) and `OSC 777 ; notify ; <title> ; <body>`
   (the terminal-notification convention). When a pane emits one, thegn mints
   a normalized attention event.
2. **A `thegn notify` CLI verb** — a process without escape-sequence support (or
   a shell hook) can shell out to raise the same event for its own worktree/pane.

Both land on the **existing** `EventBus` → notification/badge/activity-dot
plumbing, so the sidebar row, statusbar, and needs-attention queue light up with
no new consumer code. The signal is a _first-class primitive_ any program can
use, not an agent-only feature.

## Impact

- **S 251** — loop/runaway detector, **S 252** — idle vs thinking vs
  rate-limited, **S 253** — screen-phrase-matching fallback: this upgrades the
  activity-dot state machine from _heuristic inference_ to an _authoritative
  signal_ when a process opts in (heuristics remain the fallback for those that
  don't).
- **S 256** — needs-attention surfacing: an OSC/notify signal enqueues the pane
  in the existing attention queue and one-key jump (T 259).
- **AI group (notification bus)** — reuses the `EventBus`, notifications
  panel/inbox, sidebar badges, and desktop-notification derivation already
  shipped; adds only the inbound signal source.

Adds a new `attention-signals` capability. **No DB schema change** — the signal
is an ephemeral event; any persistence rides the existing notification inbox row.

## Rationale

thegn already has a rich _consumer_ side (EventBus, badges, activity dots,
notifications panel) but the only _inbound_ channels are inference-based:
CPU/activity sampling and optional screen-phrase matching. Inference is noisy
(CPU blips reset the dot — see the activity-dot sticky-state work) and can't
distinguish "waiting for the human" from "idle". Terminal emulators (Ghostty,
kitty, WezTerm) and tools already emit `OSC 9`/`OSC 777` for exactly this;
adopting the standard makes thegn a good citizen and gives agents a reliable
"raise your hand" primitive. cmux/limux validate the pattern — they draw a
notification ring on `OSC 9/99/777` — but thegn already owns the harder,
richer consumer side, so this is a small, high-leverage addition.

## Non-goals

- **A new notification schema or routing** — routing/priority is the concern of
  the separate notification-routing change; OSC signals flow through whatever
  routing exists.
- **`OSC 99` (kitty desktop-notification protocol) full parsing** — the
  multi-chunk `OSC 99` encoding is deferred; `OSC 9` and `OSC 777` cover the
  raise-attention case. (`OSC 99` may be added later behind the same event.)
- **Replacing the activity-dot heuristics** — heuristics stay as the fallback for
  processes that emit no signal; the explicit signal simply wins when present.
- **Any AI hard-dependency** — the primitive is process-agnostic; a plain build
  or `make` can use it, so the AI-free shell gains it too.
