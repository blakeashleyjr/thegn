# THE-381 plan

Scope: replace Vec::contains dedup in tree_to_worktree with an order-preserving HashSet. Test: comparison-counting linearity + order + non-UTF-8.
Out of scope: budgets, cancellation, coalescing, typed result (need THE-368/375 policy).
