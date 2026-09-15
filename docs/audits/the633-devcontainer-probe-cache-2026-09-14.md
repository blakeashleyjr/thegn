# THE-633: demand-only devcontainer capability probes

This standalone candidate is based on canonical `59a9c944` (which already
contains the landed THE617 floor extraction). It applies only the THE633
component from `774f3d78536387b6f103ad76311259448f12ce07`, followed by the
test-only Nix fixture corrections `ae68f518f0cbe04d01fb7b33a5780235ef33d948`
and `53d9a0c6f5fe8ca402497f7fc38cca52dcb30457`. No THE639 startup, monitor,
renderer, sampler, or unrelated performance implementation is included.

The repair makes capability probing demand-driven through the existing status
classifier, then uses a bounded, generation-fenced single-flight cache keyed
by executable identity, cwd, environment and relevant configuration. It
retains truthful selection/refusal precedence, separate ready/failure expiry,
changed-input and ABA invalidation, waiter re-observation, and bounded input
capture. The capability lane has independent retained subprocess custody and
does not consume Git's lane budget. Unix and Windows identity helpers validate
the selected executable safely; the Windows source is included but no Windows
native execution is claimed here.

The selected test boundary is the 19 exact probe/cache/provider/identity
selectors recorded in `the633-2026-09-15/native-pass-excerpt.log`. The
supplied combined05 full receipt identifies source
`33c0db71a3efd91a5637be47fd2d353fd628056a`, reports 8465/8465 passed and 26
skipped, and contains all 19 lines. The receipt is copied into that evidence
directory with its original SHA-256; the compact excerpt preserves the exact
pass ordinals without copying the full log.

The existing actual `build_model` fixture records nine helper calls for each
helper-present nine-build sequence before the demand gate and zero afterward
at 1, 8 and 32 private worktrees. The candidate keeps the four-line
`probes == 0` acceptance assertion for this existing fixture. The retained
measurement has a qualified hydration-only interpretation: it demonstrates
removal of unrelated probes, carries the original timing flags and repeat
disposition, and makes no release-speed, equivalent-performance, or
whole-application claim. This candidate does not rerun the ignored workload.

The source-hash manifest compares the twelve changed Rust files to actual native source `33c0db71`: eleven match exactly. `devcontainer_provider.rs` retains the pre-existing startup implementation, while tested05 also contains the separate THE-639 startup repair. The recorded source diff confirms that the probe/cache code and the nineteen selected test bodies match; migrated startup tests are excluded from this THE-633 receipt. Shared Git probe compatibility is covered by the existing full-suite result. No new full-suite execution is claimed for this extraction.

The reciprocal delivery records cover only THE-633. Historical controlled hydration measurements and the strict combined05 Clippy receipt are reused with this source bridge. Current scoped source/specification/formatting checks and reviewed local landing remain separate gates.

The durable evidence directory is `docs/audits/the633-2026-09-15/` and
contains the source hashes, full receipt copy, 19-line pass excerpt, and
qualified hydration count record. This private candidate was prepared with
source checks only; no Cargo/build/native/performance/provider process was
run, and the canonical checkout was not edited.

Remaining limits are explicit: the cache is diagnostic coherence rather than
atomic hostile same-UID executable attestation, in-place identity restoration
can remain undetected until a later demanded refresh, and Linux evidence does
not establish Windows or Darwin runtime behavior. Root owns final current
graph, native, lint, landing and closure decisions.

Primary and independent source/evidence review approved this extraction after correcting the original file-count claim. The raw reviewer report retains the earlier finding and final resolution. All durable artifact hashes were verified against their actual files. Current source ratchets, strict OpenSpec validation and corrected formatting passed. The final local-main landing remains recorded separately below.

Reviewed local-main landing completed at `ea6c2635dc16e0192c9fe60586904cb5b646f53b` on September 15. The twelve Rust file hashes and all durable evidence hashes were verified; the existing user justfile and 143 original untracked files were preserved. No new build, native run, live restart or provider operation was needed for this landing. A future PR still requires the repository pre-PR `just ci` gate; no full CI result is claimed here.
