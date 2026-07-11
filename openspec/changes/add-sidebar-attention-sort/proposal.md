# Add sidebar attention sort (surface the next thing that needs the user)

## Summary

Order worktrees (and, opt-in, workspaces) by **pending action** so the sidebar
always leads with whatever needs the user next. A pure, tiered attention model
in `thegn-core::attention` scores every worktree from signals the app
already tracks — the activity FSM, unread notifications, the PR/CI caches, and
the merge queue — into six tiers: blocked-on-user > failure > finished-awaiting
-user > ready-to-land > working > idle (longest-waiting first within a tier).

The score drives four surfaces:

1. **`SortMode::Attention`** — replaces the CPU-dot-only `Activity` mode (the
   persisted `"activity"` ui_state string parses as Attention) and becomes the
   **default** sort. Ordering follows hysteresis-stable ranks computed on the
   hydration thread: only a tier or membership change reorders; timestamp and
   cache churn never reshuffle rows.
2. **Reason hint** — the focused row's detail line spells out _why_ it floated
   ("agent needs input", "CI failed", "ready to land"), caps-aware glyphs.
3. **Jump-to-next** (`Alt a`, action `attention-next`) — focuses the most
   urgent needs-you worktree from anywhere, wrapping; works in any sort mode
   and across workspaces.
4. **Statusbar chip** (`✋ N`) — count of needs-you worktrees (tiers T0–T2),
   red when anything is blocked/failing; its detail popup lists them with
   reasons/ages and Enter focuses one.

Workspace-level bubbling is a **separate config surface**
(`[ui] sidebar_workspace_sort = "manual" | "attention"`, default manual): when
enabled, workspaces stable-sort by their most-urgent worktree's tier (equal
tiers keep the manual order). Collapsed workspaces always carry their rollup
score on the row for a glyph.

## Impact

- **tasks.md:** substantially delivers **S 256** (needs-attention surfacing)
  and **T 259** (needs-attention jump, one key) for the local-signal side.
  Complements the in-flight `add-osc-attention-signaling` proposal: explicit
  OSC/CLI signals would land as additional `AttentionInputs` later; today's
  inputs are the existing heuristics + caches.
- **Capabilities:** `sidebar` (MODIFIED default ordering + workspace-ordering
  requirements, ADDED attention requirements), `keybindings` (new default
  chord `Alt a` → `attention-next`).
- **No DB schema change.** The saved `sort_mode` ui_state value migrates by
  parse (`"activity"` → Attention); no rewrite.
- **Ratchet extractions** (mechanical, behavior-preserving): `keymap_specs.rs`
  (ACTION_SPECS out of keymap.rs), `config_ui.rs` (UiConfig out of config.rs),
  `statusbar_badges.rs` (CI/MQ chips out of chrome.rs),
  `handlers/sidebar_activate.rs` (activate_row_target out of run.rs). All four
  pinned files net-shrank; ceilings re-locked.

## Rationale

The user's recurring question all day is "what needs _me_ next?" — an agent
blocked on a permission prompt, a failed gate, a finished diff, a green PR one
keystroke from landing. All of those signals already existed in caches but
never influenced ordering; the old Activity sort ranked only by the CPU dot.
The attention model turns them into one explainable, stable ordering plus a
one-key jump, without new pollers or refresh channels (everything rides the
existing Model/Pr/Ci ticks on the hydration thread).
