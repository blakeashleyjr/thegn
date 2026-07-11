# Tasks

## 1. Detection + config merge (thegn-core)

- [ ] 1.1 Declarative known-agent table (id → binary, config path, registration
      style) + an idempotent config-merge writer that inserts/updates only
      thegn's MCP server entry — **unit tests**: merge adds the entry, re-merge
      is a no-op, unrelated entries are preserved, remove deletes only thegn's
      entry, malformed config errors cleanly.

## 2. Error markers (thegn-core)

- [ ] 2.1 A small `AgentErrorMarker` enum with stable strings + next-step guidance
      (approval-required, quota-exhausted, tool-denied, generic) — **unit tests**:
      each marker renders its stable string + guidance; unknown condition → generic.

## 3. Register/unregister verbs + marker emission (thegn-host)

- [ ] 3.1 Extend `cmd/agent.rs`: `thegn agent register` detects installed agents
      and registers thegn's MCP surface (CLI-add with config-merge fallback);
      `unregister`/`disable` removes it. Serve the existing `mcp/router.rs` surface.
- [ ] 3.2 Emit `AgentErrorMarker` strings at the bouncer approval seam and the
      proxy quota/route-failure seam in the agent-facing error text.

## 4. Docs + validate

- [ ] 4.1 Document `thegn agent register`/`unregister`, the config paths targeted,
      and the error-marker vocabulary in the agent doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
