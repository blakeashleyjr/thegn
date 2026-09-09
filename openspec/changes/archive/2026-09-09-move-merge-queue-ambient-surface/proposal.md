# Move the merge queue's ambient surface off the bottom bar

Linear: THE-9

## Why

The merge queue's ambient presence today is a statusbar chip
(`statusbar_badges.rs::push_mq_badge`: red ⚑ blocked / amber working / dim
populated) crowded into the bottom-right next to the PR, tests, loc, disk and
status widgets, while the queue's real home — the Work ▸ Merge queue panel
section in the right bar — is where every action lives (in-flight
`add-merge-queue-tui`). THE-9 asks for the ambient signal to move: out of the
bottom bar, anchored instead on the **right bar** (the panel, already the
canonical detail surface) and on a **token on the project name** — the
workspace header row in the sidebar. The token is also the _truthier_
location: the queue is per-repo (the chip already repo-scopes its counts via
`SidebarStatus::repo_scope`), so a badge on the repo's own header row says
"this project's queue" instead of a global bottom-bar number that changes
meaning as focus moves between repos.

## What Changes

- **Project-name token.** Each workspace header row in the full sidebar gains
  a right-cluster merge-queue token for that repo's queue: red with a count
  while any entry is blocked (deferred / gate_failed / gate_error /
  needs_human), amber while working (folding / verifying / agent_running),
  dim while merely populated (queued / ready), absent when empty — the same
  three-tier grammar as today's chip, reusing the shared `MqStatus` glyph
  vocabulary. It joins the existing right-cluster precedent (the warm-pool
  chip) and truncates away first when the label needs the width. Activating
  it (click, or the row's context menu / a key while the row has the cursor)
  opens the merge-queue overlay scoped to that repo; every mouse gesture
  keeps a keyboard twin.
- **Bottom-bar chip becomes an opt-in widget.** The MQ badge leaves the
  default statusbar. `[bars]` gains an `mq` widget id placeable in any slot
  (same customizable-bars machinery as `pr`/`tests`/`disk`); the default
  `bottom_right` does not include it. Users who want the old chip add
  `"mq"` back — one line of config, no code path removed.
- **The right bar stays the detail surface.** No new panel work here: the
  Work ▸ Merge queue section, the statusbar-overlay row actions and the
  per-worktree sidebar chip are `add-merge-queue-tui`'s scope; this change
  re-homes only the _ambient_ signal. On sync, `add-merge-queue-tui`'s
  "Queue state is visible outside the section" requirement is reconciled to
  name the project token (not the statusbar chip) as the ambient carrier,
  with the chip as the opt-in `[bars]` widget.
- **Rail degradation.** The 4-column rail cannot carry a counted token; the
  workspace initial's cell takes the token's urgency tint (red/amber only —
  dim stays quiet), and the full information lives one `ToggleSidebar` away.
  On terminals without the glyph repertoire, `caps::active_glyphs()` supplies
  the ASCII form; colors quantize at `wire.rs::color_spec` as always — no
  literals at draw sites (architecture ratchet).

## Impact

- **tasks.md:** group **B** item 28 (per-row badge counts — extended to the
  workspace header), group **L** items 159/160 (composable widget config,
  click-through), group **T** item 758 (merge-driver surface).
- **Capabilities:** `merge-queue` — ADDED requirement (ambient placement:
  project token, opt-in bars widget, rail tint). `sidebar` is consumed, not
  modified: the token rides the existing header right-cluster and hit-test
  geometry.
- **Depends on:** `add-merge-queue-tui` (the overlay the token opens; the
  requirement this one re-homes; `MqStatus::glyph` is shared with
  `stabilize-sidebar-internals`). Coordinates with
  `add-sidebar-visual-hierarchy` (THE-64, same header row — either order
  works; both re-record e2e, batch the re-record) and
  `rename-workspaces-to-projects` (THE-10 — "project" is that change's
  vocabulary; this spec says "workspace header" until it lands).
- **Code:** `statusbar_badges.rs` (badge → widget gating), `chrome.rs`/bars
  widget registry + `config` `[bars]` docs, `sidebar_view.rs` (header
  right-cluster + hit target), `handlers/sidebar_keys.rs`/`sidebar_mouse.rs`
  (activation), `docs/help/{bars,sidebar,merge-queue}.md`.
- **e2e:** baseline-affecting (statusbar chip disappears from default frames;
  header token appears where queues exist) — re-record with `just e2e-update`.
