# Fix sidebar drag-drop position semantics (the row-slot rule)

## Summary

Dragging a worktree in the sidebar lands it in the wrong place, and the end of a
list cannot be reached at all. The cause is a piece of geometry math that cannot
work in a terminal:

```rust
// crates/thegn-host/src/handlers/sidebar_mouse.rs (three sites)
let top_half = my < hit.y + hit.height.div_ceil(2);
```

`row_at` guarantees `hit.y <= my < hit.y + hit.height`, so for a **one-cell row**
`top_half` is unconditionally true. The row has no bottom half. Every drop on
such a row therefore resolved to "insert _before_ the hovered row" — one slot
above the aim point — and `before_pin_key: None`, the only path that appended to
the end of a run, was unreachable.

Rows are one cell in most real states: whenever the sidebar is unfocused (which
is what a drag starts in, because activating a row hands focus back to the center
pane), for `sidebar_focus_detail = "cursor"`/`"off"`, for a worktree with no
detail content, for a row clipped by the window bottom — and **always** for
workspace and folder headers, so those two axes could never reach their last
slot for anyone.

This replaces the half-row rule with the **row-slot (displacement) rule**: the
hovered row's slot is the destination, and the row it displaces shifts one step
toward where the source came from. It is the only rule that is total when a row
is one cell tall, and it makes every slot — including the tail — reachable.

Five adjacent defects found in the same audit are fixed with it: rows reflowing
under a held pointer, edge autoscroll that could not keep up with a coalesced
motion burst, a drag that could never end when its release was swallowed by a
mouse-reporting pane, an insertion rule painted under a header rather than the
subtree it lands after, and a workspace drop that step-walked and could park the
workspace between source and target.

## Impact

- Roadmap: sidebar mouse affordances — the drag-drop half of the sidebar mouse
  work proposed in `add-sidebar-actions-and-mouse`.
- Spec: `sidebar` — MODIFIED drag-drop requirement (the drop rule, tail
  reachability, drag-time geometry stability, pointer capture, and atomicity).
- Code: `crates/thegn-host/src/sidebar_order.rs` (the pure rules),
  `handlers/sidebar_mouse.rs` (the spot layer), `sidebar_view.rs` + `chrome.rs` +
  `run.rs` (the drag layout freeze), `handlers/sidebar_reorder.rs` (atomic
  workspace order), `handlers/overlay.rs` (pointer capture).
- Docs: `docs/help/sidebar.md` — the page told users to "Release between two
  rows", a gesture that never existed.

## Behaviour change

The old rule landed every drop one slot above the aim point. Anyone who
compensated for that will now overshoot in the other direction. This is
intended, and the help page and CHANGELOG say so.

Reaching the **tail of a run the source is not already in** goes through that
run's header (drop on a folder header to file at its end, or on the workspace
header to unfile at the end of the loose run) — the affordances that already
existed. Within the source's own run, the tail is just the last row.
