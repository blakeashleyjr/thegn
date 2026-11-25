# Remote fleet read through control and MCP

Expose the existing provider-backed fleet report as a read-scoped control API
capability and MCP state tool. Reuse the CLI inventory, reconciliation, cost
estimate and dispatch summary, and perform provider I/O on a blocking worker.
This change does not alter Observe or add lifecycle actions.

## Why

The fleet list and TUI popup already show provider inventory and estimated
costs, but supervisors using the control API or MCP cannot request that view.

## Scope

- Add `fleet.list` at `GET /v1/fleet` and expose it as MCP `fleet_list`.
- Return the existing list report without usage-endpoint calls.
- Keep provider inventory off the event loop and sanitize provider strings.
- Pin the route in the control schema.

The gRPC surface and a shared 30-second control cache remain follow-up work.
