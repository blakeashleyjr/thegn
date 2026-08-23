# Fix the activity signal model — dots that mean what they say

## Summary

The sidebar's activity dots are wrong in two ways users hit constantly:

- **A dot turns red while the agent is still working.** Red means "unread, look
  at me", so it fires mid-turn and the user learns to ignore it.
- **A dot turns red on a bare terminal where nothing happened.** A worktree with
  no agent at all — a plain shell — latches a permanent "needs you" alert.

Both trace to one structural gap: **the FSM has no notion of whether an agent is
involved, and no notion of which process produced a signal.** It sums CPU time
over every process whose cwd sits under the worktree, takes its second signal
from a worktree-level agent column, and arms red on a grace period that happened
to equal the poll cadence — so the grace damped nothing.

This change makes the busy signal and the alert rule mean what they claim:

1. **Red requires an agent.** A worktree positively known to have no agent shows
   white while it genuinely burns CPU, then returns to _no dot_. It can never
   latch an alert. The same `has_agent` gate already governs the statusbar
   "needs you" chip; only the dot lacked it.
2. **Quiet must be confirmed.** Arming red takes two consecutive non-busy
   observations plus the grace, at any cadence — so an agent thinking at ~0% CPU
   cannot flip the dot mid-turn.
3. **CPU is measured per process, not per sum.** A sum over the live process set
   lies in both directions: a newly-appeared process contributes its whole
   accumulated lifetime in one window (false busy), and an exiting child makes
   the sum drop to a saturating zero (false idle mid-work).
4. **An agent started by hand is recognized.** The pane predicate read the
   _spawn_ argv, so `claude` typed at a shell prompt reported `zsh` and
   contributed no output signal — leaving CPU alone to judge an agent that uses
   ~0% CPU while waiting on a model.
5. **Solicited repaints stop looking like agent work.** A resize SIGWINCHes every
   pane and full-screen TUIs redraw; a daemon reattach replays scrollback. Both
   arrive through the same path as live output.
6. **"Finished" and "blocked on you" are different dots.** They were both the
   same red, so a completed turn shouted as loudly as an agent stuck on a
   question.

Every threshold becomes `[activity]` config, which also makes
`platform-windows`'s existing promise of a "configured cooldown" true.

## Impact

- Roadmap: hardens **item 20 / 425** (contextual tree dots) and the **item 256**
  needs-attention surfacing they feed.
- The inference heuristic remains the fallback that
  `add-osc-attention-signaling` is designed to supersede; this change makes that
  fallback trustworthy rather than replacing it.
- Carries forward the OSC-title requirement stranded in the archived
  `2026-08-14-add-agent-observability` change (archived with every task
  unchecked; `classify_title` was never implemented). This change does **not**
  implement title classification either — it removes the need for it on the
  blocked-vs-finished axis by deriving that distinction from the attention tier,
  which is fed by real signals. The archived `SHALL` is superseded, not silently
  abandoned.

## Non-goals

- The declared-signal protocol (OSC 9/777, a `thegn notify attention` verb) —
  that stays with `add-osc-attention-signaling`.
- Per-pane dots. The per-pane attribution this change computes is staged for the
  daemon's `WaitCondition::{Idle,Blocked,Done}`, still unimplemented.
- Re-keying the sidebar's activity map from tab name to worktree path. Two
  worktrees sharing a tab name still collide; the loop's `Loading`/`Failed`
  overlays are keyed by tab name too, so that is its own change.
