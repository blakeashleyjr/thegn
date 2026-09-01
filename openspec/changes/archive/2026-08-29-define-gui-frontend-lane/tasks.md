# Tasks — define-gui-frontend-lane

## 1. Publish the decision

- [x] 1.1 Record the not-now decision, verified substrate gap matrix, three
      candidate shapes, preferred candidate 2, and reopen criteria in the
      dated superpowers design.
- [x] 1.2 Record the one-catalog, 0%-idle, shell-independence, security-edge,
      and substrate-boundary invariants.
- [x] 1.3 Coordinate with THE-34's filter/lag vocabulary without designing a
      second event protocol, and define THE-40-F1 as the observer cell-client
      contract follow-up.

## 2. Synchronize and archive

- [x] 2.1 Rewrite proposal, design, tasks, and architecture-gates delta as a
      documentation-only decision record.
- [x] 2.2 Archive the synchronized change under the dated OpenSpec archive
      path and remove the active duplicate.
- [x] 2.3 Leave dependency bans, crate-boundary ownership,
      `docs/ARCHITECTURE.md`, roadmap, code, config, catalog, API, database,
      tests, and ratchets to separate future implementation work.

## 3. Validate

- [x] 3.1 Run `just openspec-validate` and `git diff --check`.
- [x] 3.2 Run the required scoped architecture smoke checks:
      `just quick thegn-core`, `cargo nextest run -p thegn-core capability`,
      `just quick thegn-host`, and `cargo nextest run -p thegn-host help`.
