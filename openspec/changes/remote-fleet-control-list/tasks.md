- [x] Add the read-scoped `fleet.list` capability and `GET /v1/fleet` route.
- [x] Reuse and sanitize the existing list report on a blocking worker.
- [x] Add the MCP `fleet_list` projection and update the wire route snapshot.
- [ ] Add isolated control and MCP fixture coverage; run it in the coordinator's
      build environment. The coverage is written and staged in this worktree
      (`control/tests.rs`, `mcp/state.rs` tests, `cmd/fleet.rs` tests);
      `cargo` cannot run in this lane, so the coordinator's build runs them.
- [x] Prove two provider accounts and an unreachable fake provider produce the
      expected rows and partial timestamped result without real credentials.
      Synthetic composition in `cmd/fleet.rs` tests: the duplicate row
      collapses, the unreachable env lands in `view.partial` with its reason,
      and the explicit `generated_ms` and sanitized rows are asserted. The
      two-account and unreachable _join_ halves stay pinned by
      `join_orphan_missing_duplicate_and_partial` and
      `env_inventory_shapes_each_read`.
- [x] Decide shared cache and gRPC projections against the current Linear
      acceptance: they are not in it — the acceptance names the control API and
      MCP read surfaces. Both stay follow-up work per `proposal.md` and
      `docs/rfcs/fleet-list-control.md`.
