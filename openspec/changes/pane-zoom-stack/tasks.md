# Tasks

## 1. Per-tab zoom

- [x] 1.1 `center::Grow` and a `Tab::grow` field (not persisted), set in every
      constructor.
- [x] 1.2 `pane_zoom::cycle_or_zoom` cycles the active tab's level; the loop
      local keeps only the sidebar/panel zone zoom.
- [x] 1.3 `pane_zoom::effective_zoom` plus a loop-top derive that recomputes
      the chrome whenever the grown zone changes.
- [x] 1.4 Activation repairs only a stale focus
      (`sidebar_activate::repair_stale_focus`).

## 2. Stack rendering

- [x] 2.1 `CenterTree::Stack` layout carves collapsed one-row bars, via
      `stack_bars` and `activate_stack_member`.
- [x] 2.2 `pane_zoom::grown_tree` returns a stack of every pane with the
      focused one active; `displayed_tree` is shared by relayout, render and
      mouse.
- [x] 2.3 `borders::draw_stack_bars`, drawn by `chrome::render_panes`.

## 3. Input

- [x] 3.1 `overlay::pre_dispatch` hit-tests the displayed tree;
      `MousePre::StackBar` expands the clicked pane.
- [x] 3.2 `pane_zoom::nav_layout`: vertical moves walk the stack while zoomed.

## 4. Docs and validation

- [x] 4.1 Help page: the Zoom section in `terminal-and-panes.md`.
- [x] 4.2 Unit tests for the stack geometry, the Grow cycle, the effective
      zoom, the nav layout, the bar rendering, and the activation round-trip.
- [ ] 4.3 Pre-push gate (`just test`, clippy, smoke).

## 5. Queue-review repairs (THE-559 / THE-560)

- [x] 5.1 THE-559: gate stack activation on the running app owner, the same
      splash predicate as rendering, and painted overlay occlusion. Preserve
      existing modal, drawer, and pointer-capture priority.
- [x] 5.2 THE-559: run actual `pre_dispatch` regressions for visible stacks,
      running app takeover, splash, overlays, held presses, drawer and capture.
- [x] 5.3 THE-560: register real issue/project owners in both delivery ledgers,
      preserving active/verification-gated status and these unchecked gates.
- [x] 5.4 THE-560: run delivery validation, negative fixtures and source ratchets
      without weakening the guards.
- [ ] 5.5 THE-559 / THE-560: review and verify the candidate integrated with
      current main; run the remaining full gates and required visual/e2e
      evidence before queue approval or issue closure.

## 6. Full-feature review repairs (THE-566 / THE-567 / THE-568)

- [x] 6.1 THE-566: share stack activation between keyboard directional focus
      and mouse activation, preserving the original stack's active member.
- [x] 6.2 THE-567: omit bars that cannot be painted at widths below two cells,
      using the shared layout for both rendering and mouse dispatch.
- [x] 6.3 THE-568: compute paired frame/bar geometry in one visitor pass for
      rendering and hit-testing; single-output callers allocate only their
      requested output.
- [ ] 6.4 Run the keyboard restore, narrow-bar dispatch and paired-geometry
      regressions along with the prior focused suite, without weakening gates.
- [ ] 6.5 Review actual-source visual evidence and the fully integrated gates;
      no merge approval or tracker closure before these pass.
