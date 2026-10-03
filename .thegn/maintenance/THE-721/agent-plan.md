# THE-721 plan

Scope: notification_sound.rs only. The fallback latch (fallback_bell + waker, consumed by notify::take_bell on the loop) already existed for no-provider cases. Gaps: queue-full, legacy command failure, and provider failure (incl. timeout) did not all latch. Add them (shutdown Cancelled excluded). Coalescing is the single AtomicBool latch consumed once per flush; no timers. The bell is written by the loop, never the worker.
Tests: error mapping, failed command, full queue. Out of scope: new config key.
