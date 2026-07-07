# Tasks — semantic blast-radius subsystem

## 1. Pure core (superzej-core, 95%-gated)

- [ ] 1.1 `entity_id(repo, file, qualified_name, kind)` stable hash join key in
      `semantic.rs` (or a `semantic_graph.rs` sibling). Unit test: identical
      inputs → identical id; kind/name/file changes → different id.
- [ ] 1.2 `map_reference_to_entity(refs, entities_by_file)` — resolve caller
      locations to enclosing `entity_id` via `Entity::contains`. Unit tests:
      location inside an entity resolves; module-level / out-of-span location is
      dropped; overlapping/nested spans pick the innermost.
- [ ] 1.3 `classify_coverage(edges, entities)` — test-kind caller ⇒ callee
      covered; changed entity with no test caller ⇒ untested. Unit tests over a
      graph with and without test callers.
- [ ] 1.4 `risk_score(changed, blast)` → `low|medium|high`, total + monotonic.
      Unit tests pin the threshold bands (fan-out, untested count, touch-kind).
- [ ] 1.5 `BlastRadius` summary type (changed / callers / files / untested /
      risk) with a render helper shared by the footer and MCP tool; unit-tested.

## 2. Persistence (store seam + user_version bump)

- [ ] 2.1 `store/semantic.rs` — `trait SemanticStore` (upsert entities for a
      file, replace edges for source ids, load callers of an id, load entity for
      a `(file, span)`); register in `store/mod.rs`.
- [ ] 2.2 `db_semantic.rs` — SQLite impl on `Db` mirroring `db_proxy.rs`; create
      `sem_entity` + `sem_edge` (+ indexes).
- [ ] 2.3 **Bump `db::SCHEMA_VERSION` 37 → 38**; add `migrate_v38` creating the
      two tables; update the schema round-trip + migration-rung tests so
      `ver == SCHEMA_VERSION` holds on a fresh and an upgraded DB.
- [ ] 2.4 Core unit tests for the store impl round-trip (upsert → load callers →
      replace edges → re-load) against an isolated temp DB.

## 3. LSP references sourcing (superzej-svc / host, off-loop)

- [ ] 3.1 Confirm `LspClient::references(uri, pos)` returns caller locations
      through the warm `LspSupervisor::client(root, lang)` path; resolve the
      **feasibility caveat** (server support, warm-index completeness). If
      `references` is insufficient, add `callHierarchy/incomingCalls` to
      `svc/lsp` — tracked here as the fallback.
- [ ] 3.2 Host graph builder: for each changed entity, obtain the warm client,
      query `references`, normalize LSP line/char ↔ entity line ranges at the
      edge, and write entities + edges via `SemanticStore`. Runs on
      `spawn_blocking` / the fs-watcher thread — never on the event loop.

## 4. Incremental invalidation (fs-watcher)

- [ ] 4.1 Ride the existing diff fs-watcher: re-parse only files whose
      `source_hash` changed, rewrite edges touching their entities, then pulse
      the `TerminalWaker`. No new wake path, no polling timeout.
- [ ] 4.2 Skip-on-unchanged-hash fast path; drop entities that vanished from a
      re-parsed file.

## 5. Surfaces

- [ ] 5.1 Footer: `hydrate.rs` `compute_entity_summary` augments the
      `EntitySummary` with `BlastRadius` when a graph exists →
      "N changed · C callers/F files · U untested · risk:R"; unchanged intra-diff
      string when absent.
- [ ] 5.2 `blast_radius` MCP house tool beside `semantic_diff` in
      `mcp/router.rs` (no-args, connection-worktree scoped); returns changed
      entities + callers + untested + risk.
- [ ] 5.3 Expose the `Risk` band as a serializable review-gate signal (consumed
      by the T266 change-explanation pipeline; this change only emits it).

## 6. Graceful degradation

- [ ] 6.1 `[lsp]` off / no server / unknown language → no edges, `BlastRadius`
      `None`, footer falls back to today's intra-diff summary, MCP tool returns
      "graph unavailable". Unit-test the pure fallback (empty graph → intra-diff
      summary unchanged).

## 7. Docs + validation

- [ ] 7.1 Document any new `[lsp]`/semantic keys in `config/config.toml.example`.
- [ ] 7.2 `smoke.sh`: build the graph over a fixture worktree and assert the
      `blast_radius` MCP tool + footer degrade cleanly with LSP off.
- [ ] 7.3 Run `just ci` (fmt-check + lint + build + test + coverage + smoke +
      nix-build + openspec-validate) — the pre-PR gate.
