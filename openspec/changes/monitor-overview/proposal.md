# Monitor Overview tab

Linear: THE-871

Add an Overview landing tab to the system monitor and dispatch it through the
F6b board. The view summarizes the latest collector frame alongside the
existing masthead snapshot, while legacy tabs keep their current render path.

## Motivation

The board has placement, demand, focus, and zoom behavior, but every tab still
renders as a legacy section stack. Overview establishes the first real board
view and gives later view modules a stable dispatch seam.

## Scope

Overview is first and is selected by default for new users. Existing persisted
tab slugs continue to restore their matching tab. Panels cover CPU, load and
pressure, memory health, network, disk, thermal, optional power, and process /
container counts. Frame data is copied into overlay-owned buffers before the
sampler recycles its frame. Pausing freezes that copy.

Drill-down, reuse of later tab painters, and MonitorHistory graphs are deferred.
