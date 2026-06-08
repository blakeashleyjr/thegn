# szhost chrome stability design

Date: 2026-06-07
Status: approved (committed)

## Goal

Fix the two remaining regressions after the `szhost` performance/jank pass:

1. tab labels briefly flash at the far left of the top chrome before moving to the
   workspace/center area; and
2. the right panel becomes wider after changing workspaces or tabs.

The fix should make chrome geometry stable across first paint, hydration, tab
switches, palette tab navigation, new/close tab flows, and ordinary pane relayout.

## UX contract

- The tabbar background remains full width from column `0` to the terminal width.
- Tab labels align with the workspace/center column, not with the far-left edge of
  the terminal when the sidebar is visible.
- Tab labels draw inside the same horizontal bounds as the active workspace pane:
  `chrome.center.x .. chrome.center.x + chrome.center.cols`.
- The right panel keeps the configured panel width for a given screen size and
  sidebar/panel toggle state. Switching tabs or workspaces must not widen or
  shrink it.
- Resizing the terminal and toggling sidebar/panel visibility are the only normal
  interactions that recompute chrome geometry.

## Chosen approach

Keep the existing fixed chrome cross and add a stable tabbar content rectangle
that is derived from the center pane:

```text
tabbar_content.x    = chrome.center.x
tabbar_content.y    = chrome.tabbar.y
tabbar_content.cols = chrome.center.cols
tabbar_content.rows = chrome.tabbar.rows
```

Rendering then uses two separate regions:

1. `chrome.tabbar` for the full-width background fill; and
2. `chrome.tabbar_content()` for tab labels.

A method on `ChromeLayout` is preferred over a stored field because it is derived
state, keeps `layout::compute` less invasive, and prevents field drift between the
center and the tabbar label region.

## Architecture details

### Layout API

Add `ChromeLayout::tabbar_content(&self) -> Rect`.

The method returns a rectangle aligned to `center` but on the tabbar row. This
captures the product decision that labels belong to the workspace area while the
background remains full chrome width.

Expected examples:

- wide screen with sidebar and panel visible:
  - `tabbar.x == 0`, `tabbar.cols == screen cols`;
  - `tabbar_content.x == SIDEBAR_COLS`;
  - `tabbar_content.cols == center.cols`;
  - `panel.cols == PANEL_COLS`.
- wide screen with sidebar hidden:
  - `tabbar_content.x == 0`;
  - `tabbar_content.cols == center.cols`.
- repeated `compute(cols, rows, same toggles)`:
  - panel width remains exactly `PANEL_COLS` when visible;
  - center and tabbar content geometry are identical across calls.

### Tabbar rendering

Change `draw_tabbar` so it receives both the full tabbar rectangle and a content
rectangle:

- return early if the full tabbar has zero rows;
- fill the full tabbar background exactly as today;
- draw labels starting at `content.x + 1`;
- cap labels at `content.x + content.cols`;
- do not draw labels in the sidebar-owned far-left columns when the sidebar is
  visible.

`draw_chrome` should call `draw_tabbar(surface, chrome.tabbar, chrome.tabbar_content(), model)`.

### Stable panel width across switches

Tab/workspace switch paths should not recompute chrome geometry. They should only:

1. update the session/model active tab state;
2. mark panes for relayout against the already-current `chrome.center`; and
3. mark the frame dirty.

The intended behavior is already mostly present in `run.rs`; the implementation
should make it explicit and regression-tested. Only these paths should assign a
new `chrome = layout::compute(...)`:

- initial startup;
- sidebar toggle;
- panel toggle;
- terminal resize.

Tab switching, palette tab navigation, new tab, close tab, split/focus actions,
and model hydration must reuse the current `ChromeLayout` snapshot.

### Render consistency

Pane materialization, pane relayout, pane composition, cursor placement, and
chrome drawing must all use the same `chrome.center` for a frame. This prevents a
first-frame conceptual mismatch where tab labels are drawn relative to one region
and panes/panel are later drawn relative to another.

## Tests

Add headless unit tests before production changes:

1. `ChromeLayout::tabbar_content` aligns to the center on a wide screen with
   sidebar and panel visible.
2. `tabbar_content` starts at column `0` when the sidebar is hidden.
3. Repeated layout computations with identical screen/toggle inputs preserve the
   panel width and the center/tabbar-content geometry.
4. `draw_tabbar` fills the full tabbar but draws labels only inside the center
   content area when the sidebar is visible.
5. `draw_chrome`/full-frame rendering places the active tab label in the center
   tabbar content area rather than the far-left columns.
6. A pure helper or focused `run.rs` test documents that tab switches reuse the
   current chrome layout instead of recomputing it.

## Verification

Run the requested stack after implementation:

```sh
nix fmt
just fmt-check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just lint
just coverage
just smoke
```

If any gate fails due to an environment blocker rather than the change, record the
exact blocker and run the strongest available focused checks.

## Non-goals

- Replacing the native host event loop.
- Reworking sidebar/panel toggle policy or auto-hide thresholds.
- Changing tab naming, palette item ordering, or session persistence.
- Implementing the full native host substrate roadmap.
- Changing the right panel contents; only its geometry stability is in scope.

## Success criteria

- On first paint and subsequent paints, tab labels appear in the center/workspace
  area with no far-left flash.
- With a stable terminal size and panel toggle state, switching tabs/workspaces
  leaves the right panel at the configured width.
- Existing launch-performance improvements remain intact.
- The full verification stack is green, or any non-code environment blocker is
  reported with evidence.
