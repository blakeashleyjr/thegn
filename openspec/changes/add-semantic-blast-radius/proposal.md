# Add the semantic blast-radius subsystem (inter-entity impact graph)

## Summary

Today the `◈ semantic` footer answers "_what did this diff change?_" — it runs
`semantic::entities_for_diff` / `impact_summary` per-diff on the hydration thread
(`hydrate.rs` `compute_entity_summary`) and reports the changed
functions/structs _within_ the patch. It cannot answer the question that makes a
review actionable: "_who depends on what I changed, and is any of it
untested?_"

This change evolves that intra-diff footer into an **inter-entity blast-radius**
subsystem: a persistent, incrementally-maintained entity graph whose edges are
_caller → callee_ relationships, so a diff can report its downstream fan-out.

1. **Edges come from the LSP, not hand-rolled name resolution.** For each
   changed entity the subsystem queries `textDocument/references` through the
   already-warm `LspSupervisor`, maps each reference location back to the entity
   that contains it, and records a `sem_edge(caller → callee)`. Name resolution,
   overloads, generics, and cross-file resolution are the language server's job,
   not ours.
2. **Pure core, coverage-gated; I/O at the edges.** The mapping of
   reference-locations → `entity_id`, the test-coverage classification (a caller
   whose `EntityKind` is a test ⇒ the callee is _covered_; a changed entity with
   no test caller ⇒ _untested_), and the risk score are **pure functions** in
   `semantic.rs` / a sibling core module with unit tests to the 95% core gate.
   The `references` calls and re-parses run **off the event loop** on the
   hydration / fs-watcher threads, reusing the warm supervisor.
3. **Persistence via the store seam.** A new `SemanticStore` trait
   (`store/semantic.rs`) + SQLite impl (`db_semantic.rs`) mirror the existing
   13-domain store pattern (`store/proxy.rs` + `db_proxy.rs`). Two tables —
   `sem_entity(id, file, name, kind, span, source_hash)` and
   `sem_edge(src_id, dst_id, kind)` — behind a **`user_version` bump 37 → 38**.
4. **Incremental, off-loop invalidation.** The graph rides the existing diff
   fs-watcher: on a file change only files whose `source_hash` changed are
   re-parsed and their edges rewritten, then the `TerminalWaker` is pulsed. The
   graph is _never_ rebuilt on the event loop.
5. **Three surfaces, one signal.** (a) the `◈` footer enriches to
   "_3 changed · 14 callers/6 files · 2 untested · risk:high_"; (b) a
   `blast_radius` MCP house tool sits next to `semantic_diff` in
   `mcp/router.rs`; (c) the risk score is a signal the review-gate / change-
   explanation pipeline (T266) can consume.
6. **Strictly additive graceful degradation.** With `[lsp]` disabled, or no
   server for the diff's language, the subsystem contributes no edges and the
   footer falls back to today's intra-diff summary. The AI-free shell never
   hard-depends on the graph.

## Impact

- tasks.md: directly advances **X (313 Impact/blast-radius analysis, 316 inspect
  risk scoring)** in the Semantic git layer, building on the already-shipped
  `semantic.rs` primitives (**X 309 sem-core, 311 entity-level diffs, 312 entity
  blame, 317 entity-derived commit messages**). It feeds the review track — the
  risk signal is what **T 266 (AI change explanation, sem + LLM)** and **T 265**
  consume — and the multi-forge review callouts **AT (643 review-focus / risk
  callouts, 644 review-plan assistant)**. It does not implement weave/semantic
  merge (**X 314, T 270**), which remain separate.
- **Capabilities** — ADDS a new `semantic-graph` capability (graph build,
  incremental invalidation, pure blast-radius/coverage/risk, footer enrichment,
  `blast_radius` MCP tool, no-LSP degradation). No existing capability spec is
  removed; the `panel` footer behavior it enriches is captured here.
- **superzej-core** — new `store/semantic.rs` trait + `db_semantic.rs` SQLite
  impl (registered in `store/mod.rs`); a `user_version` 37 → 38 migration
  (`SCHEMA_VERSION` const + `db_migrate` + the schema-round-trip test); new pure
  functions in `semantic.rs` (or a `semantic_graph.rs` sibling) for
  location→entity mapping, coverage classification, and risk scoring, all
  unit-tested to the 95% gate; a `blast_radius` arm in `mcp/router.rs`.
- **superzej-svc** — `LspClient::references` already exists
  (`svc/lsp/mod.rs:770`); this change _uses_ it. The only svc risk is
  confirming rust-analyzer returns caller ranges reliably enough to resolve back
  to entities (see design's feasibility caveat).
- **superzej-host** — an off-loop graph builder/invalidator wired to the diff
  fs-watcher and hydration path (reusing `LspSupervisor::client(root, lang)`);
  `hydrate.rs` `compute_entity_summary` enriches the footer from the graph when
  present. **No new event-loop wake path** — producers pulse the existing
  `TerminalWaker`; the footer update is the existing hydration → chrome `dirty`
  path, not a new tick.

## Rationale

The `semantic.rs` extractor already yields precise entity spans per file, and
the `LspSupervisor` already keeps a warm, indexed language server per
`(root, lang)`. The missing middle was a persistent join of the two: turn "these
entities changed" into "these entities are _reached by_ the change" by asking the
one component that actually resolves names — the LSP. Persisting the graph (and
invalidating it incrementally on the fs-watcher we already run) means the
blast-radius is available at footer speed without any per-diff whole-repo
re-analysis, and the pure classification stays in the coverage-gated core where
it can be locked by tests.

## Non-goals

- **Hand-rolled cross-file name resolution.** Edges are LSP-sourced only; a
  language with no server contributes no edges (degrades to the intra-diff
  footer). We do not reimplement a resolver.
- **Runtime / dynamic call graphs.** Edges are static `references`, not traced
  executions; reflection and dynamic dispatch are out of scope.
- **Semantic merge / weave** (**X 314, T 270**) — the graph informs review; it
  does not drive merges.
- **A whole-repo graph pre-build on launch.** The graph is built lazily and
  incrementally from changed files; there is no blocking full-repo indexing pass.
- **Making any of the AI-free shell hard-depend on the graph or the LSP** — the
  footer, panel, and CLI all work unchanged when `[lsp]` is off.
