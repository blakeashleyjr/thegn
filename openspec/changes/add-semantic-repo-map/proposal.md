# Semantic repo map — outlines the worktree already knows, exposed

Linear: THE-25

## Why

THE-25 asks "Code outlines?" seeded by the tree-sitter repo maps Aider and
Goose inject into agent context. The audit answer: **per-file** outlines
already exist and are good — the Symbols panel section renders a
document-symbol outline (LSP `documentSymbol` with tree-sitter entity
fallback), and Search Everywhere's symbol mode aggregates workspace symbols.
What thegn does **not** have is the thing the linked tools actually ship: a
**repo-level, ranked, budget-bounded map** of a worktree, consumable outside
the interactive panel — by an agent over MCP, or by a script over the CLI.

Two substrate findings shape the scope:

- **The semantic store is diff-scoped today.** `sem_entity`/`sem_edge`
  (schema v42, behind the `SemanticStore` seam) hold only entities changed in
  `git diff HEAD` plus the caller entities that reference them
  (`blast_radius.rs`, 50-file cap). There is no worktree-wide entity index to
  render a map from — so this is _mostly_ an exposure problem, but not
  purely: the index must first grow from "changed entities" to "the
  worktree", which the existing incremental machinery (fs-watcher +
  `source_hash` skip) already knows how to keep fresh.
- **Spec drift found during the audit**: the `semantic-graph` spec requires a
  `blast_radius` MCP house tool "in the MCP router", and code comments still
  cite it — but no such tool exists. The MCP router it targeted was excised
  with the AI layer; today's `thegn mcp serve` carries docs tools plus four
  no-argument state tools. This change re-anchors that requirement onto the
  real MCP server instead of leaving a phantom claim in the spec.

Ranking needs no new analysis: the graph's caller→callee edges give an
in-degree signal (the moral equivalent of Aider's graph-centrality ranking)
wherever LSP references have been mapped, with a deterministic structural
fallback everywhere else.

## What Changes

- **Worktree-wide entity index.** The persistent entity store grows from
  diff-scoped to worktree-wide for tree-sitter-tier languages: an initial
  off-loop crawl per worktree (capped; oversized worktrees degrade to
  partial coverage), kept fresh incrementally by the existing fs-watcher +
  `source_hash` skip. No language server required — parsing is tree-sitter;
  edges remain LSP-sourced and optional, per the existing spec.
- **A pure repo-map renderer in thegn-core.** Rows (kind, name, file, span)
  ranked by caller in-degree where edges exist, with a deterministic
  kind/file-order fallback; rendered under a line budget,
  most-important-first, grouped by file. Pure functions over owned rows,
  unit-tested to the core gate.
- **`thegn map`** — CLI verb: human-readable map of the current (or named)
  worktree, `--json` through the shared emitter, `--budget` for the line
  budget, `--file <path>` narrowing to one file's outline.
- **MCP exposure via the catalog.** Two `thegn_core::capability::CATALOG`
  rows — `semantic.map` (new) and `semantic.blast_radius` (re-homing the
  spec's phantom tool) — projected as read-scope MCP tools on
  `thegn mcp serve`, taking worktree/budget arguments. Rows claim only the
  surfaces they implement (no new `SURFACE_GAPS` excuses).
- **Symbol-search fallback upgrade.** Search Everywhere's symbol mode reads
  the entity index for tree-sitter languages when no LSP answer is
  available, ahead of the regex sweep.
- **Spec honesty:** the `blast_radius` MCP requirement is MODIFIED to
  describe the real server and scope-gated projection.

## Impact

- **Roadmap**: extends group **X** (semantic git layer — items **313/316**
  gave the graph its store and risk logic); strengthens **AQ 523/531**
  (Search Everywhere symbols / outline views). The exposure story replaces
  what the excised AI-layer context injection would have consumed — kept
  strictly generic (any MCP client, any script).
- **Specs**: `semantic-graph` (ADDED worktree index, repo-map rendering, CLI
  - MCP exposure, symbol fallback; MODIFIED `blast_radius` MCP requirement).
    No new capability directory.
- **Code (indicative)**: `thegn-core/src/store/semantic.rs` +
  `db_semantic.rs` (list/degree queries, possible additive index →
  `user_version` bump), new `thegn-core/src/repo_map.rs` (pure renderer),
  `thegn-host/src/blast_radius.rs`-adjacent index crawler,
  `thegn-host/src/cmd/map.rs`, catalog rows in
  `thegn-core/src/capability.rs`, MCP tool wiring in `thegn-core/src/mcp/` +
  `thegn-host/src/cmd/mcp.rs`, `search_everywhere.rs` fallback.
- **Config**: `[semantic]` section — `worktree_index` (master switch,
  default on), `index_max_files`, `map_budget_lines` — documented in
  `config/config.toml.example`.
- **In-flight changes**: **depends on** the MCP write-tools branch
  (parameterised state tools + `--scopes` gate) for argument-taking MCP
  tools — the tools here are read-scope and slot into that substrate, not a
  second one. Reconciled with `complete-control-surface-coverage` (new
  catalog rows claim only implemented surfaces; the coverage ratchet must
  not grow), `add-cli-namespaces-and-remote-open` (the `map` verb slots
  into whatever namespace grouping that change lands),
  `add-viewers-and-quick-open` (no overlap — panel/quick-open surfaces are
  untouched here). `add-generic-lsp-registry` is a sibling, not a
  dependency: the map is tree-sitter-tier by design.

## Non-goals

- PageRank-style centrality (in-degree + deterministic fallback first;
  fancier ranking is a drop-in behind the pure seam).
- Signature text in map rows (needs source reads in the renderer; deferred —
  rows carry kind/name/file/line, which is what navigation and ranking need).
- Growing the tree-sitter grammar set, or any LSP-registry coupling.
- A new panel surface — the Symbols section already covers interactive
  per-file outlining.
