# THE-640 process sort direction review

Issue: [THE-640](https://linear.app/blakeashley/issue/THE-640/align-processes-pidname-sort-order-with-the-displayed-direction-arrows)
Reviewed: 2026-09-15
Candidate base: `060afad525a14547336e8d2170803a66af877812`

The Processes view applies one shared descending reversal to the comparator
used by both flat and tree rows. CPU and RSS already return ordinary ascending
comparisons. Name and PID returned reversed comparisons, causing the `↓`
heading state to display A-to-Z and low-to-high values. The bounded candidate
changes only those two comparator expressions and their comment. Name retains
the existing lexical String ordering; no case-folding behavior is introduced.

The focused regression fixture uses shuffled rows `(3, alpha, 30.0, 300)`,
`(17, middle, 10.0, 100)`, and `(42, zulu, 20.0, 200)`. It asserts both
directions for CPU, memory, Name, and PID, and the host monitor fixture checks
the actual `cpu`, `mem`, `name`, and `pid` heading labels with `↓`/`↑` after
the existing c/m/n/p/r keys. Tree coverage checks root and sibling ordering,
parent-before-child traversal, filtered ancestry, and cycle/orphan retention.
The confirmation check sorts before opening `x`, then changes a sampled row's
rank and verifies the pending `(pid, start_time)` identity without confirming
or dispatching a signal.

Root review requires the old-comparator counterfactual to remain in a separate
private clone while its source-only before/after assertions execute. That
counterfactual must fail the Name and PID direction assertions while CPU and
RSS continue to pass; it must not run a live process action. Full Cargo/native,
adversarial, and local-main integration evidence remains root-owned.
