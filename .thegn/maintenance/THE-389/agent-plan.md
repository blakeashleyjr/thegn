# THE-389 plan

Scope: thegn-svc git diff-stat accounting only (mod.rs parse_numstat/sum_numstat, native_diff.rs totals).

- Strict shared parser `parse_numstat` (path required, only `-\t-` is binary and omitted like native, non-decimal/oversized = Err, digit-bounded).
- Saturating totals in one shared `sum_entries`, used by both backends.
- Non-zero git exit in numstat reads is an Err, not 0/0.
- Tests: malformed, boundaries, aggregate overflow, binary, native-vs-CLI typed rows.
  Out of scope: widening to u64 / typed Exact|Capped|Binary|Unavailable model and UI states (needs model/UI design decision; wide fan-out), THE-380/381/383 items.
