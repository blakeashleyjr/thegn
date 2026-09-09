# Decide bounded plugin sidebar, theme, and key-zone surfaces

Linear: THE-107 (Active, Plugin UI Platform)

## Why

`SidebarTab` and `Theme` exist as plugin wire vocabulary without a general-host
runtime, while plugin key zones have no settled contract. Calling the UI "fully
pluggable" would therefore be false. Each surface has different ownership,
security, placement, input, and performance constraints and needs a bounded
adopt/defer/reject decision.

## What Changes

- Produce and publish a decision matrix for `SidebarTab`, `Theme`, and plugin
  key-zone contributions.
- Define the minimum host contract and acceptance gates for any adopted surface.
- Keep reserved/absent surfaces rejected by negotiation and described honestly.
- Create separate implementation changes only for adopted surfaces.

PanelSection negotiation/render/cache/placement is exclusively THE-108 and is
out of scope here. General v0.3 docs/host truth alignment is THE-106.

## Impact

- Specs: new `plugin-ui-boundaries` support-state contract.
- No runtime extension point, config key, keybinding, theme mutation, or render
  path is added by this decision change.
- A future adopted child change must update plugin API/runtime and relevant
  sidebar/theming/keybinding specs atomically.
