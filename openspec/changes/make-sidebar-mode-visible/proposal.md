# Make the persistent sidebar view mode visible

Linear: THE-93

## Why

Bare `g` changes and persists flat/grouped mode, but the sidebar header exposes
sort state only. After the transient message disappears, an intentionally flat
tree is indistinguishable from missing grouping data.

## What Changes

- Project flat/grouped state into the frame/sidebar render model.
- Render a persistent `FLAT` chip in the full sidebar using the established
  sort-chip treatment; grouped/default mode and the slim rail remain uncluttered.
- Keep keyboard, help, filter-header composition, narrow-width behavior, and
  persistence in one tested contract without adding a false hit target.

## Non-goals

- Changing the `g` binding or the meaning/default of flat mode.
- A new grouping engine.
- Adding mode text to the intentionally label-free slim rail.
