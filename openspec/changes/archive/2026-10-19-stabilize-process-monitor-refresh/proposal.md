# Stabilize process monitor refresh

THE-628 tracks the reported task manager process table flicker, while THE-640
tracks the related displayed-direction mismatch for Name and PID. Every model
hydration replaces the sampler-owned process snapshot with its empty default;
the next independent process sample restores it. Tied top-N selection can also
change membership with OS enumeration order, and passive re-sorts move the
selection to a different process at the same row index.

Preserve the last process snapshot at the authoritative hydration swap, make
bounded admission and row ordering deterministic, make every sort key agree
with its displayed direction, and preserve sampled process identity during
passive refresh and pending signal confirmation. This repairs existing behavior
without adding controls, sampling faster, or new features.

Impact: roadmap item 412 (per-process attribution and monitor Processes tab),
`system-monitor`, host model publication/monitor, and metrics process sampler.
Linear THE-628 and THE-640; coordinated review and delivery registration under
THE-614.
