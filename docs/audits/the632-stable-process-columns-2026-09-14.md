# THE-632: stable process columns

This receipt describes the source candidate stage. The current-main port is
based on 6884c3f0 and remains unaccepted pending native integration, paired
performance and final review gates.

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

Pending: final independent review, host-native regression tests, paired release
refresh/render measurements, owned Muse frame checks, combined repository gates
and local-main landing. THE-638 tracks the selected maintenance batch.
