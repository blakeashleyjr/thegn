# Tasks — semantic repo map

## 1. Store queries (thegn-core)

- [ ] 1.1 `SemanticStore` additions: `entities_under(root_prefix)` and
      `caller_degrees(dst_ids | all)` (in-degree per callee), implemented in
      `db_semantic.rs`. Add the `sem_entity(file)` index if the query plan
      wants it — additive migration + `user_version` bump v42 → v43.
- [ ] 1.2 CRUD + migration-ladder tests for the new queries (95% core gate;
      DB tests isolate `XDG_STATE_HOME`).

## 2. Config (thegn-core)

- [ ] 2.1 `[semantic]` section: `worktree_index` (default true),
      `index_max_files`, `map_budget_lines`. Wire through `config_validate`.
- [ ] 2.2 Document every key in `config/config.toml.example` (the generated
      config-reference help page picks them up).
- [ ] 2.3 Unit tests: defaults, round-trip, validation.

## 3. Pure renderer (thegn-core)

- [ ] 3.1 New `repo_map.rs`: ranked row model (kind, name, file, line,
      degree), in-degree ranking with the deterministic structural fallback
      (kind weight → path → line), and the line-budget emitter grouped by
      file.
- [ ] 3.2 Serializable row form for `--json` / MCP results.
- [ ] 3.3 Exhaustive unit tests: determinism (same input twice ⇒ identical
      output), edge-less fallback order, budget smaller than one file, empty
      index, partial-index marker.

## 4. Index builder (thegn-host)

- [ ] 4.1 Off-loop worktree crawl (git-listed tree-sitter-tier files →
      `parse_entities` → `replace_file_entities`), capped by
      `index_max_files`, thread QoS `Background`, first-open trigger.
- [ ] 4.2 Incremental refresh riding the existing fs-watcher trigger with
      the `source_hash` skip and the graph builder's debounce pattern.
- [ ] 4.3 Waker pulse only when the focused file's fallback outline data
      changed (no new wake path or damage channel — assert via the existing
      render-plan tests staying green).

## 5. CLI (thegn-host)

- [ ] 5.1 `cmd/map.rs`: `thegn map [--worktree <path>] [--budget N]
[--file <path>] [--json]`; inline capped crawl when the index is empty
      and no compositor owns the worktree; `--json` through the shared
      emitter.
- [ ] 5.2 Smoke-test the verb (hermetic repo fixture; `-c
commit.gpgsign=false` in fixtures).
- [ ] 5.3 Add the verb to the `docs/help/cli.md` namespace table (reconcile
      placement with `add-cli-namespaces-and-remote-open` if it has landed).

## 6. Catalog + MCP (thegn-core / thegn-host)

- [ ] 6.1 Catalog rows `semantic.map` and `semantic.blast_radius` with
      `required_scope = read`, surfaces claimed = exactly those implemented;
      the per-verb pinned-count tests and coverage tests updated — **no new
      `SURFACE_GAPS` entries**.
- [ ] 6.2 MCP tools on `thegn mcp serve` taking `worktree` +
      `budget`/`file` arguments — **rebase on the parameterised-state-tools
      branch** (MCP write-tools work); read-scope gating via the existing
      `--scopes` mapping. `blast_radius` returns changed entities, callers,
      untested set, risk band; "graph unavailable" when absent.
- [ ] 6.3 Router unit tests: scope masking hides both tools under
      `--scopes none`; argument validation errors are JSON-RPC errors.

## 7. Symbol-search fallback (thegn-host)

- [ ] 7.1 Search Everywhere symbol mode consults `entities_under` for the
      active worktree when LSP yields nothing, before the regex sweep;
      off-loop like the existing symbol workers.
- [ ] 7.2 Targeted test: LSP-less tree-sitter worktree still answers symbol
      queries from the index.

## 8. Docs + validate

- [ ] 8.1 Update `docs/help/` prose for the new verb and config keys (no
      new action/keybind/zone/panel section ⇒ no help-ratchet entries
      expected; verify `just test` help ratchets stay green).
- [ ] 8.2 `git add` new modules before nix-build (flake source allowlist).
- [ ] 8.3 Run `just ci` once, when the change is complete (includes
      openspec-validate).
