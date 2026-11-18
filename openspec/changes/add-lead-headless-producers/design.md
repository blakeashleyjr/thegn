# Design

## Decisions

- **One owner, lease per kind.** CAS in `producer_leases`, holder `daemon`,
  TTL three cadences (at least 60 s).
- **Arming by need.** Inert unless the master key is on and something needs it.
- **UI yields, never leases.** The UI reads the lease and skips the network
  fetch while the daemon holds it; duplicates are deduped by `once_per_key`.
- **Autopilot pickup** runs inside the tracker tick and keeps its sealed sandbox.
- **CI polling is not moved.** `pr_checks` comes from the PR poll's rollup.
