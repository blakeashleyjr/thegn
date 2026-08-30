# Chunk 1 — document existing agent memory and close THE-49

## Scope

This is the sole, independent documentation chunk for THE-49. Record the
decision that thegn owns no new memory engine or index, document the existing
dispatch/pipeline/tracker/cache/session read paths with file:line evidence,
evaluate memex and the dead MCP-proxy draft, and preserve a tightly gated
follow-up contract for a possible future `memory.search` capability.

## Files touched

- `.thegn/pipeline/THE-49/architect/design.md` — the sole decision record.

No Rust, config, openspec spec, help page, database schema, ratchet, provider,
MCP, daemon, or platform file may be touched by this chunk.

## Approach

1. Treat the current branch, `CLAUDE.md`, and `docs/ARCHITECTURE.md` as
   authority; treat `openspec/changes/add-mcp-proxy-hub/` as a stale draft to
   verify and prune, not as an implementation plan.
2. Define memory as cross-worktree/cross-session readable context and map the
   current roster, `dispatch report`, `dispatch note`, git-backed artifacts,
   Linear comments, `thegn agent sessions`, and SQLite cache boundaries.
3. Close with “no new system.” Keep memex and harness memory files optional and
   outside thegn. State that any future search must be optional, partitioned by
   profile/workspace, offline, on-demand, catalog-only, edge-degraded, and
   pure/ranked/tested in `thegn-core`.

## Overlap and dependencies

No overlap or dependency with another chunk: this is the only chunk, and it
touches only the decision record. The Lead may run it in parallel with no code
work; there is no implementation ordering requirement.

## Tests to run

This is documentation-only, so no behavior test is expected. Run the scoped
checks required by the repo workflow if the development environment is already
available:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core pipeline_report`

Do not run `just test`, `just ci`, e2e, or a full-workspace build for this
chunk. A reviewer should also inspect `git diff --check`, verify that no
ratchet/config/schema files changed, and confirm that any `thegn` invocation
uses a temporary `XDG_STATE_HOME`.

## Done criteria

- `design.md` states and justifies “no new memory system” and does not extend
  the dead MCP-proxy design.
- The current existing memory surfaces and their limitations are enumerated
  with file:line citations, including `dispatch report`, `dispatch note`,
  artifacts, Linear comments, SQLite cache, and `thegn agent sessions`.
- memex is evaluated as an optional harness-layer tool; no vendor is embedded
  or made a hard dependency.
- The future `memory.search` contract includes optionality, profile/workspace
  partitioning, no network/wake source, catalog-only exposure, edge
  degradation, and pure unit-tested ranking policy.
- No implementation/config/schema/ratchet/help files are changed.
- The coder must commit with this exact subject: `docs(the-49): architect design + chunk specs`.
