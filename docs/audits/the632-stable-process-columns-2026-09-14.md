# THE-632: stable process columns

This receipt describes the fixed-column design. The current-main port starts
from 6884c3f0. Current native, UI and release evidence, including retained p95
qualifications and the native landing boundary, is recorded in
`task-manager-maintenance-THE-630-632.md`.

The Processes table now allocates PID, name, owner, CPU and memory widths from
the viewport. A new opt-in fixed table section clips each cell before drawing
it at its column position. Dynamic tables retain their existing sizing path.
Tree indentation is bounded by the name column, full 32-bit PIDs receive ten
cells where the viewport permits, and clipped fields carry the terminal's
ellipsis marker. Selection and confirmation retain the sampled PID and birth
time; no process signal is sent by the fixtures.

The fixed renderer uses the same `finl_unicode` grapheme boundaries and termwiz
cell widths as the Surface it writes. The dependency was already locked
transitively through termwiz; it is now explicitly declared by the host.
Control/line-movement and bidi formatting clusters become spaces. Standalone
zero-width clusters become one space rather than attaching to the preceding
column. Fitting combining and emoji sequences remain whole. Palette tokens,
selection gutter and background remain the existing monitor vocabulary.

The following review and four-test receipt are historical evidence recorded
with the original fixed-column implementation at `671db7cc`, not execution
against the current-main port. Sagan independently approved the plan. Dalton
reviewed production source and
requested exact Unicode-name visibility assertions, which were added. Four
actual-source renderer tests passed against cached real termwiz, using fixture
palette and section-dispatch seams. This is early renderer evidence, not the
complete host integration or a release performance result. The host test also
checks the actual Processes builder across narrow/wide viewports and changing
sample values, including full PID visibility and unchanged confirmation identity.

The current-source verification includes 68 focused host tests and owned Muse
frames at three sizes. Release comparisons support lower measured median and
allocation cost, with individual p95 regressions retained explicitly in the
maintenance audit. Native queue receipts supply the final repository gate and
local-main landing result. THE-638 tracks the selected maintenance batch.
