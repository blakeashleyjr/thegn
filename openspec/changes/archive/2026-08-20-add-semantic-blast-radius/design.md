# Design — semantic blast-radius subsystem

## The pure / I/O split

The subsystem is deliberately halved along the core's substrate-agnostic seam so
the interesting logic is unit-testable and coverage-gated:

- **PURE (thegn-core, 95%-gated).** All of it takes owned data and returns
  owned data — no LSP, no DB, no clock:
  - `entity_id(repo, file, qualified_name, kind)` — a stable content-free hash
    that is the join key across parses. Two parses of an unchanged entity yield
    the same id.
  - `map_reference_to_entity(refs, entities_by_file) -> Vec<entity_id>` — given a
    set of `references` locations and the per-file entity spans (reusing
    `Entity::contains`), resolve each caller location to the entity that
    encloses it. Locations that fall in no entity (module-level, comments) are
    dropped.
  - `classify_coverage(edges, entities) -> Coverage` — an edge whose _caller_
    entity `kind` is a test (`EntityKind` test variant) marks its callee
    _covered_; a changed entity with zero test callers is _untested_.
  - `risk_score(changed, blast) -> Risk` — folds fan-out breadth (callers /
    distinct files), untested count, and change touch-kind into a
    `low|medium|high` band. Monotonic and total, so the tests can pin exact
    thresholds.
  - `BlastRadius` summary (changed count, caller count, file count, untested
    count, risk) that the footer and MCP tool both render.
- **I/O (thegn-svc / thegn-host, excluded from the core gate, exercised by
  smoke).** The `references` LSP round-trips and the SQLite reads/writes. These
  run on the hydration `spawn_blocking` path and the fs-watcher thread — never on
  the event loop.

## Where the edges come from (LSP `references`) + feasibility caveat

For each changed entity we take its definition span, ask the warm client for
`textDocument/references` at the definition position, and feed the returned
caller locations into `map_reference_to_entity`. The host already owns exactly
the right handle: `LspSupervisor::client(root, lang) -> Arc<LspClient>` (warm,
lazily spawned + initialized off-loop, gated by the `[lsp]` master switch), and
`LspClient::references(uri, pos)` already exists (`svc/lsp/mod.rs:770`, parsed by
the shared `Location | Location[] | LocationLink[]` decoder).

**Feasibility caveat (call out as a risk / task):** although `references` exists
on `LspClient`, it has _not_ been driven through the supervisor's warm-client
path in host code before — the only consumers so far are `document_symbols`,
`definition`, and `hover`. Two things must be validated during implementation:

1. **Server support & shape.** Not every server implements `references`, and
   rust-analyzer needs its index warm before results are complete; a cold or
   partial index yields _fewer_ edges, never wrong ones — acceptable, but the
   footer must treat an empty/partial result as "graph thin here", not
   "no callers". If `references` proves too coarse we may additionally need
   `callHierarchy/incomingCalls`; that is an svc addition, tracked as a risk.
2. **Location → position round-tripping.** Reference locations are LSP
   line/char; entity spans are byte/line ranges from `parse_entities`. The
   mapping must reconcile the two coordinate systems (the pure mapper takes
   already-normalized line ranges so the conversion lives at the I/O edge and is
   smoke-tested).

If the client can't be obtained (LSP off, no server, spawn failure) the builder
records no edges and the footer degrades — see Graceful degradation.

## Persistence + the user_version bump

New store domain, mirroring `store/proxy.rs` + `db_proxy.rs`:

- `store/semantic.rs` — `trait SemanticStore` (object-safe, `&self` + concrete
  args): upsert/replace entities for a file, replace edges for a set of source
  ids, load callers of an `entity_id`, load the entity for a `(file, span)`.
- `db_semantic.rs` — the embedded-SQLite impl on `Db`, registered in
  `store/mod.rs`.

Schema (two tables):

```sql
CREATE TABLE sem_entity (
  id          TEXT PRIMARY KEY,   -- hash(repo, file, qualified_name, kind)
  file        TEXT NOT NULL,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL,
  span        TEXT NOT NULL,      -- serialized line range
  source_hash TEXT NOT NULL       -- hash of the file's source at parse time
);
CREATE INDEX sem_entity_file ON sem_entity(file);

CREATE TABLE sem_edge (
  src_id TEXT NOT NULL,           -- caller entity_id
  dst_id TEXT NOT NULL,           -- callee entity_id
  kind   TEXT NOT NULL,           -- 'ref' | 'call' | 'test'
  PRIMARY KEY (src_id, dst_id, kind)
);
CREATE INDEX sem_edge_dst ON sem_edge(dst_id);   -- "who calls me"
```

**SQLite `user_version` bump — REQUIRED.** `db::SCHEMA_VERSION` goes **37 → 38**;
a `migrate_v38` creates the two tables, and the schema round-trip test
(`assert_eq!(ver, SCHEMA_VERSION)`) plus the migration-rung test are updated. Per
the DB-is-a-cache rule the tables are pure derived state — a fresh DB simply
rebuilds them from the fs-watcher, so no backfill is needed on upgrade.

## Incremental invalidation on the fs-watcher

The graph rides the **existing diff fs-watcher** (the same discipline that
already filters `.git/` events and does recursive inotify registration off a
background thread). On a batch of changed paths:

1. For each changed source file, hash its contents; if `source_hash` matches the
   stored row, skip it (no re-parse).
2. For files whose hash changed: `parse_entities` → upsert `sem_entity` rows
   (removing entities that vanished), then re-query `references` for the changed
   entities and `replace` the `sem_edge` rows whose `src_id`/`dst_id` touch them.
3. Pulse the `TerminalWaker` so the next drain re-hydrates the footer.

All three steps run **off the event loop** on the watcher / hydration threads.
The loop only ever _reads_ the resulting `BlastRadius` when composing the footer.

## Render damage channel + event-loop wake path

Per the config rules this must be explicit:

- **No new wake path and no new damage channel.** The blast-radius footer is
  produced on the **hydration thread** exactly like today's
  `compute_entity_summary`; the result is delivered over the existing hydration
  mpsc channel and the producer **pulses the existing `TerminalWaker`**.
- On drain, an updated footer sets the master **`chrome` dirty** flag (the same
  channel the current `◈ semantic` footer uses) — a chrome/overlay change, so
  `render_plan::plan()` yields **`Full`**, unchanged from today. Pane content is
  untouched, so pane-only frames stay **`Panes`** and an idle wake stays
  **`Skip`** — the render-decision invariants and their tests are unaffected.
- No polling timeout is added anywhere; the idle loop still blocks on
  `poll_input(None)`.

## Surfaces

- **Footer.** `hydrate.rs` `compute_entity_summary` gains a graph lookup: when a
  graph is present for the diff's `(root, lang)`, it augments the existing
  `EntitySummary` with `BlastRadius` and renders
  "_N changed · C callers/F files · U untested · risk:R_"; absent a graph it
  renders exactly today's intra-diff string.
- **MCP tool.** A `blast_radius` house tool beside `semantic_diff` in
  `mcp/router.rs` (same no-args, connection-worktree-scoped dispatch shape):
  returns the changed entities plus their callers, untested set, and risk. It is
  advisory context for a review agent (T266).
- **Review-gate signal.** The `Risk` band is a plain serializable value the
  change-explanation / review pipeline can read; this change _emits_ it and does
  not itself build the gate.

## Graceful degradation (hard rule)

The graph is strictly additive. When `[lsp].enabled` is false, or
`LspSupervisor::client` returns no server / errors for the language, or the diff
is in a language `semantic::Lang::from_path` doesn't recognize: no edges are
written, `BlastRadius` is `None`, and every surface falls back — the footer to
today's intra-diff summary, the MCP tool to a "graph unavailable" result, the
review signal to absent. Nothing in the AI-free shell hard-depends on the graph.
