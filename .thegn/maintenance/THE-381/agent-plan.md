# THE-381 plan

Scope: replace Vec::contains dedup in tree_to_worktree with an order-preserving HashSet. Test: comparison-counting linearity + order + non-UTF-8.
Out of scope: budgets, cancellation, coalescing, typed result (need THE-368/375 policy).
Deferred items 2-6 (budgets, cancellation, coalescing, typed result, per-file cap) are blocked on THE-368, THE-375 and THE-259.
