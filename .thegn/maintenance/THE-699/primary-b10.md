# THE-699 — primary instructions (batch 10, codex restart)

Commit b620d68bd fixed the mouse-wheel scroll fast path (wheel sets its own scroll_dirty; fast path only
when render_plan::wheel_is_sole_damage(&damage); refused wheel folds into Damage::chrome => Full;
second-pane wheel => Full; wheel_delta_rows clamps 1..=8 ticks x 5 rows). An adversarial review was
CLEAR. A previous agent died mid-way applying three follow-ups; its UNCOMMITTED changes may be partial —
`git diff` first, keep what is right, finish the rest:

1. Reset `scroll_pane = None` next to `scroll_only = false` (~crates/thegn-host/src/run.rs:13992) so the
   second-pane => Full check only fires within one frame, matching its comment.
2. Stop draining wheel events at WHEEL_MAX_TICKS and leave the rest queued (instead of discarding them),
   so a fast flick keeps its distance spread over frames while each frame's jump stays bounded. If the
   drain helper (drain_event_repeats ~run.rs:4864) cannot stop early without reordering other input,
   keep the cap and say so in your report.
3. Extract the gate into crates/thegn-host/src/render_plan.rs as a pure fn (e.g.
   `scroll_fast_ok(scroll_only, &Damage, guards) -> bool`) plus the refused-wheel fold, call it from
   run.rs, and add tests that encode the bug main had: scroll_only + another pane's output => refused
   and plans Full; scroll_only + empty damage => fast path.
   Keep render_plan::plan unchanged and every existing render_plan invariant test meaningful
   (idle wake => Skip; pane output only => Panes; chrome/overlay/geometry => Full).
