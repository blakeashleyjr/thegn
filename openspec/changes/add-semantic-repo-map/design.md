# Design — semantic repo map

## What exists today (audited)

- **Per-file outline**: Symbols panel section (`panel/sections/symbols.rs`)
  renders LSP `documentSymbol` with tree-sitter `parse_entities` fallback;
  fetches run off-loop (`run.rs::spawn_outline_fetch`, outline channel).
- **Symbol search**: Search Everywhere symbol mode is LSP-first with a regex
  sweep fallback.
- **Semantic store**: `sem_entity` (id, file, name, kind, span,
  source_hash) + `sem_edge` (caller→callee) behind the object-safe
  `SemanticStore` seam (schema v42). Populated **diff-scoped only**: the
  blast-radius builder writes entities for changed files (50-file cap) and
  upserts caller entities from referencing files. Incremental machinery
  (fs-watcher trigger, `source_hash` skip, delete-then-insert per file)
  already exists.
- **Exposure**: none outside the compositor. The spec's `blast_radius` MCP
  house tool does not exist — the router it targeted was excised with the AI
  layer. `thegn mcp serve` today: docs tools + four no-argument state tools
  (`sessions_list`, `worktrees_list`, `leases_list`, `me`).

## Index builder

One new off-loop job per worktree, sharing the blast-radius builder's
skeleton (spawn_blocking, `Db::open`, `SemanticStore` writes, source-hash
skip):

- **Initial crawl** on first worktree open (or first `thegn map` /
  MCP call when the compositor is not running): walk tree-sitter-served
  files under the root (respecting the git listing, not raw readdir, so
  ignored/vendored trees are skipped), parse with `parse_entities`, write
  rows via `replace_file_entities`. Capped by `[semantic] index_max_files`
  (default generous); beyond the cap the index is honestly partial and every
  reader says so.
- **Incremental**: rides the existing diff fs-watcher trigger exactly like
  the graph builder — changed path, hash differs ⇒ re-parse that file only;
  vanished entities drop via delete-then-insert. Debounced with the same
  pattern as `BUILD_DEBOUNCE`.
- **QoS**: the crawl thread declares `Background` (housekeeping), per the
  platform::qos rule for new long-lived work.
- **No LSP involvement.** Edges stay the graph builder's job; the index is
  pure tree-sitter. Registry-only languages (see `add-generic-lsp-registry`)
  are absent from the index by construction.

## Pure renderer (`thegn_core::repo_map`)

Input: owned `SemEntityRow`s + an in-degree table (`dst_id` → caller
count, from a new store query). Output: ranked, budgeted map text and a
serializable row form.

- **Ranking**: in-degree descending; ties and edge-less worktrees fall back
  to a deterministic structural order (kind weight — types/traits before
  functions before consts — then file path, then line). Same-input ⇒
  same-output, pinned by tests like `risk_score`'s determinism scenario.
- **Budget**: a line budget (`--budget` / `map_budget_lines`, default ~200);
  files are emitted whole-or-elided headers with their top entities,
  most-important-first, until the budget closes. The budget math is pure and
  unit-tested (empty index, single file, budget smaller than one file).
- Rows carry kind label, name, `file:line`. No source reads in core
  (substrate-free); signature text is a deferred host-side enrichment.

## Exposure surfaces

- **CLI**: `cmd/map.rs` — resolves the worktree (cwd or `--worktree`), reads
  the store, renders. `--json` goes through the shared emitter (cli spec's
  machine-readable requirement). If the index is empty and no compositor is
  running, the verb runs the crawl inline (it is the CLI process's own
  time), honoring the cap.
- **MCP**: two catalog rows, `semantic.map` and `semantic.blast_radius`,
  `required_scope = read`, surfaces claimed = exactly those implemented
  (CLI + MCP; HTTP/gRPC/plugin only if trivially projected — no new
  SURFACE_GAPS excuses, per `complete-control-surface-coverage`). The MCP
  tools take `worktree` and `budget`/`file` arguments — which requires the
  parameterised-state-tools substrate from the in-flight MCP write-tools
  branch; this change layers read tools on it and does not re-scope it.
- **Symbol-search fallback**: symbol mode consults the index (name-prefix /
  fuzzy over `sem_entity` rows for the active worktree) when LSP yields
  nothing, before the regex sweep. Off-loop like the existing symbol
  workers.

## Event loop / rendering

No new damage channel and no new wake path. The index builder runs off-loop
and does not render; it pulses the existing waker only when the Symbols
panel's fallback data for the focused file changed (same contract as the
outline channel). CLI/MCP surfaces run outside the compositor loop
entirely. Render decision (`Skip`/`Panes`/`Full`) is untouched.

## SQLite

Reuses `sem_entity`/`sem_edge` unchanged in shape. New read queries (list
entities under a root prefix, in-degree per `dst_id`) may want an index on
`sem_entity(file)` — if added, it is an **additive migration with a
`user_version` bump** (v42 → v43). Rows remain derived state: a fresh DB
rebuilds from the crawl, no backfill.

## Security

- **Read-only surfaces.** No new write door anywhere: the CLI verb and both
  MCP tools only read derived rows from the state DB (and, for the inline
  crawl, source files the invoking user can already read).
- **Scope model**: both catalog rows carry `required_scope(read)`; `thegn
mcp serve --scopes none` serves docs tools only, hiding them — matching
  the existing state-tool gating. No credential material is touched; no
  SecretRef surface.
- **Blast radius of exposure**: a repo map is a structural summary of source
  the caller can already open — same trust domain as `worktrees_list` plus
  file reads. Remote surfaces inherit the pairing/scope checks of the
  control plane; nothing here weakens them.
- **Sandbox**: the crawl parses files with tree-sitter in-process; it MUST
  respect the git file listing (no wandering into symlinked trees outside
  the root).

## Alternatives considered

- **Render the map from LSP `workspace/symbol`** — rejected: needs a warm
  server per language, empty on LSP-less hosts, and gives no ranking signal;
  the tree-sitter index works everywhere the grammars do.
- **A new `repo-map` capability directory** — rejected: the behaviour is the
  semantic graph's store and surfaces; one capability, extended.
- **PageRank ranking (Aider-style)** — deferred: in-degree over real
  reference edges is deterministic, cheap, and already better than
  file-order; the ranking function is a pure seam a better algorithm can
  replace without touching surfaces.
- **Injecting the map into agent prompts** — out of scope by construction:
  the AI layer is excised; exposure is generic (MCP/CLI) and anything
  agent-shaped consumes it from the outside.

## Open questions

- Should the initial crawl also run headlessly on `thegn wt new` (warm the
  index before first open), or only on first open / first query? Leaning
  first-open to keep worktree creation snappy.
- In-degree counts today would weight test callers equally;
  `is_test_entity` could discount them. Deferred until the ranking is felt
  in practice.
