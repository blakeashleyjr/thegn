# Thegn

- Work in the issue’s worktree. Linear owns scope and status. `just agent-context THE-123` finds specs and handoffs. Update OpenSpec when behavior changes.
- Trace callers with CodeGraph in the current worktree or `just graph-find <symbol>`. Use `rg` for literals.
- Check the [extension recipe](docs/extending/README.md) before changing a surface. Keep core substrate-free and blocking I/O off the event loop.
- In `nix develop`, run `just quick <crate>` and `cargo nextest run -p <crate> <filter>`. Follow [merge gates](CONTRIBUTING.md#dev-loop).
- Cite the commit and source path in AgentMemory (`project=thegn`). Record decisions, test results and open questions in the issue handoff.
