# THE-265 / THE-266 plan

- 265: retry relaunch always cold (no continue_last); native id persistence needs a schema change, out of scope. Recovery note bounded.
- 266: receive loop constant-time; bounded per-session retry tasks (8 concurrent, 64 pending), dup coalescing, run-ref fence after backoff, lag/overflow reconcile from roster+tombstones, attempts GC.
- Tests: cold spec unit test; backoff-isolation and lag-reconcile daemon tests.
