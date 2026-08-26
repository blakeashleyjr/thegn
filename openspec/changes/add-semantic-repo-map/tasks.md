# Tasks — semantic repo map

## 1. Store queries (thegn-core)

- [x] 1.1 `SemanticStore` additions: `entities_under(root_prefix)` and
      `caller_degrees()` (in-degree per callee), implemented in `db_semantic.rs`.
      **No `user_version` bump needed** — the query plan is served by the
      existing `idx_sem_entity_file` / `idx_sem_edge_dst` indexes (see `db.rs`
      DDL), so no additive migration was required.
- [x] 1.2 CRUD tests for the new queries (prefix path-boundary anchoring,
      LIKE-wildcard escaping, distinct-caller counting) — in-memory DB, isolated.

## 2. Config (thegn-core)

- [x] 2.1 `[semantic]` section: `worktree_index` (default true),
      `index_max_files` (5000), `map_budget_lines` (200). Validated by the
      schema walk (unknown-key + type checks) by construction — no enums added,
      so the pinned enum count is unchanged.
- [x] 2.2 Documented every key in `config/config.toml.example`.
- [x] 2.3 Unit tests: defaults, round-trip, floors, unknown-key validation.

## 3. Pure renderer (thegn-core)

- [x] 3.1 New `repo_map.rs`: ranked row model (kind, name, file, line, degree),
      in-degree ranking with the deterministic structural fallback (kind weight
      → path → line → name), line-budget emitter grouped by file.
- [x] 3.2 Serializable `MapRow` for `--json` / MCP results.
- [x] 3.3 Exhaustive unit tests: determinism, edge-less fallback order, budget
      smaller than one file, empty index, partial-index marker survives a tiny
      budget, degree-marker-only-when-nonzero.

## 4. Index builder (thegn-host)

- [x] 4.1 Off-loop worktree crawl (`repo_index.rs`): git-listed tree-sitter
      files → `parse_entities` → `replace_file_entities`, capped by
      `index_max_files`, thread QoS `Background`, per-root first-open trigger.
- [x] 4.2 Incremental refresh: the crawl's `source_hash` skip re-parses only
      changed files; the blast-radius builder already keeps diff files fresh on
      the same hash. Debounced (20s) + always-on-first-visit per root.
- [x] 4.3 Waker pulse only when the crawl wrote changes (no new wake path /
      damage channel — the crawl never renders; render-plan tests untouched).

## 5. CLI (thegn-host)

- [x] 5.1 `cmd/map.rs`: `thegn map [--worktree <path>] [--budget N]
[--file <path>] [--json]`; inline capped crawl when the index is empty;
      `--json` through the shared `emit_json` emitter.
- [x] 5.2 Smoke-tested against a hermetic repo fixture (see `test/smoke.sh`).
- [x] 5.3 Added the verb to `docs/help/cli.md` + `docs/cli.md` namespace tables
      and the `--json` list.

## 6. Catalog + MCP (thegn-core / thegn-host)

- [x] 6.1 Catalog rows `semantic.map` (Cli + Mcp) and `semantic.blast_radius`
      (Mcp) with `required_scope = read` (new `Verb::SemanticMap` /
      `Verb::SemanticBlastRadius`), surfaces claimed = exactly those
      implemented — **no new `SURFACE_GAPS` entries**. Pinned-count tests updated
      (verb scope table, MCP state-cap split, CLI/MCP coverage lists).
- [x] 6.2 MCP tools on `thegn mcp serve` taking `worktree` + `budget`/`file`
      arguments, layered on the parameterised-state-tools substrate; read-scope
      gating via the existing `--scopes` mapping. Both answer daemon-free from
      the state DB + git. `blast_radius` returns changed entities, callers,
      untested/risk (via the new pure `compute_blast_report`); "graph
      unavailable" when absent.
- [x] 6.3 Router coverage: the existing scope-masking + argument-validation
      router tests (`thegn-core::mcp::state`) cover the new read tools by
      construction (they run over `MCP_STATE_CAPS`).

## 7. Symbol-search fallback (thegn-host)

- [x] 7.1 Search Everywhere symbol mode consults the index (`entities_under`)
      ahead of the regex sweep, merged (index-first) then LSP-first on upgrade;
      off-loop in the existing `spawn_symbol_search` worker.

## 8. Docs + validate

- [x] 8.1 Updated `docs/help/cli.md` + `docs/cli.md` for the new verb, config
      keys documented in `config.toml.example` (no new action/keybind/zone/panel
      section ⇒ no help-ratchet entries).
- [x] 8.2 `git add` the new modules before nix-build (flake source allowlist)
      — done at review time.
- [ ] 8.3 Run `just ci` once when the change is complete (pre-PR gate).
