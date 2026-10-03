# THE-699 plan

Scope: run.rs wheel path + render_plan.rs.

1. Wheel set shared `dirty`, so the scroll fast path could not tell sole-damage from stale-elsewhere; it skipped other pending damage then cleared it. Give wheel its own `scroll_dirty`; fast path requires `render_plan::wheel_is_sole_damage`; otherwise fold into chrome (Full).
2. Wheel over a second pane before the frame overwrote scroll_pane (first pane's scroll lost): force Full.
3. Bound coalesced ticks (`wheel_delta_rows`, max 8 ticks x 5 rows).
   Out of scope: measuring ghostty events/notch, sidebar/panel paths (already full-frame via dirty/sidebar_dirty), PTY byte capture.
   Tests: render_plan pure tests, run_tests wheel_delta_rows.
