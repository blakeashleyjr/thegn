# THE-640 durable evidence checkpoint

This compact checkpoint preserves the reviewed THE-640 evidence at private
checkout `cc3a61142d9a26c2c949e50f8fc7398b1e991125`. Root's canonical source
merge is `b025ec5d877f191d65cc831c95035c493c49c7f6`. The positive receipt
identifies tested source `9553d36878b8e4726524997d2cd7e05e3b4b1f93`; the two
production source hashes are recorded in `manifest.json` and remain unchanged.

The positive native gate is complete: the pinned actual host test binary ran
14 exact selectors and passed 14/14. The counterfactual gate is also complete:
build 27339 exited 0, and its three exact selectors each produced the intended
Name-direction failure (exit 101). The first Name panic stopped later PID
assertions in those test processes; that statement is based on the captured
log. CPU/RSS prelude assertions passed where reached. No live process action was
used. The final adversarial review is complete.

The scoped production-bin Clippy gate is complete with exit 0. Its admission,
log, and CPU-quota readback are included. The command requested taskset CPU 3
for the counterfactual build, but the execution environment broadened affinity
to CPUs 0–21; the manifest makes no CPU 3 enforcement claim. Clippy's quota
readback records the one-scope 100% aggregate CPU cap and no affinity claim.

The `.raw` files preserve logs, receipts, build admissions, and independent
source/native reviews. Binaries, depfiles, and counterfactual build JSONL are
excluded; original paths and hashes remain in the copied reviews/receipts and
manifest. THE628 shared tasks and pending performance work are preserved.

The corrected repository audit is
`docs/audits/process-sort-THE-640.md`; its current SHA-256 is recorded in the
manifest. Landing and closure remain pending root review. No landing approval
is implied by this checkpoint.
