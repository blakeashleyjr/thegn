# Design

Carry the existing persisted `ViewState.flat` boolean through `SidebarState`
into `FrameModel`; the renderer never owns or persists a second mode value.

In the full sidebar, flat mode reserves a compact `FLAT` chip beside the
existing sort/hold chip. Grouped/default mode renders no structural-mode chip,
and toggling back removes `FLAT` in the same frame. The chip remains present
while the filter input owns the header, using the same semantic colors and
right-edge clipping policy as the sort chip. Under constrained width, labels
yield before the chip; no text may spill or manufacture a clickable region.

The indicator is informational, not a new mouse control. Bare `g` continues to
own the only toggle and existing persistence/cursor/sort-freeze behavior. The
slim rail renders no mode word because its labels are intentionally absent.
