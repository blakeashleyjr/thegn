# THE-433 plan

Scope: font picker discovery off the compositor, bounded.

- font.rs: typed Discovery/DiscoveryStatus, fc-list via preview_jobs::bounded_capture (3s, 4MiB, group kill+reap, cancel), admitted exe resolution from fixed system dirs (no PATH) via platform::exe_admitted, bounded macOS dir scan (entries/name bytes/time/cancel/depth, errors reported), result cap, single-flight DiscoveryOwner with generation tags (Drop cancels).
- run.rs: SwitchFont requests the owner; result drained from a channel (worker pulses waker), palette opens only for accepted generation.
  Out of scope: result cache (optional in the issue), measured macOS latency budget.
  Tests: font.rs unit tests (hang, flood, cancel, breadth, deadline, unreadable, resolver, generations).
