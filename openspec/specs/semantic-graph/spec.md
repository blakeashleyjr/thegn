# semantic-graph Specification

## Purpose

TBD - created by archiving change add-semantic-blast-radius. Update Purpose after archive.

## Requirements

### Requirement: The entity graph is built from LSP references off the event loop

thegn SHALL maintain a persistent entity graph whose edges are caller→callee
relationships, and it MUST source those edges from the language server's
`textDocument/references` (never from hand-rolled name resolution) using the
warm per-`(root, lang)` LSP client. Every reference query, parse, and database
write for the graph MUST run off the event loop (on the hydration or fs-watcher
threads); the loop MUST NOT block on graph work.

#### Scenario: A changed entity's callers become edges

- **WHEN** the graph builder processes a changed entity in an LSP-served language
- **THEN** it queries `references` at the entity's definition, maps each caller
  location back to the entity that encloses it, and records a
  `caller → callee` edge in the graph

#### Scenario: Graph work never runs on the loop

- **WHEN** the graph is built or updated
- **THEN** the parse, the `references` calls, and the SQLite writes run off the
  event loop and signal completion by pulsing the `TerminalWaker`, and the loop
  only reads the resulting summary when composing chrome

### Requirement: The graph is invalidated incrementally on source-hash change

thegn SHALL update the graph incrementally by riding the existing diff
fs-watcher: on a file change it MUST re-parse only files whose stored
`source_hash` differs from the file's current contents, and rewrite only the
edges touching those files' entities. A file whose hash is unchanged MUST NOT be
re-parsed, and entities that vanished from a re-parsed file MUST be removed.

#### Scenario: Only changed files are re-parsed

- **WHEN** the fs-watcher reports a batch of changed paths
- **THEN** files whose `source_hash` matches the stored row are skipped and only
  the files whose contents changed are re-parsed and have their edges rewritten

#### Scenario: A removed entity drops its edges

- **WHEN** a re-parse of a file no longer contains a previously-recorded entity
- **THEN** that entity's row and the edges touching it are removed from the graph

### Requirement: Blast-radius, coverage, and risk are computed by pure tested logic

thegn SHALL compute the blast-radius summary — caller count, distinct caller
files, untested set, and a risk band — from pure functions in thegn-core that
take owned data and perform no I/O, and these functions MUST be unit-tested to
the core coverage gate. An edge whose caller entity is a test MUST mark its
callee covered; a changed entity with no test caller MUST be reported untested.
The risk band MUST be a total, deterministic function of the fan-out, untested
count, and change kind.

#### Scenario: An untested changed entity is flagged

- **WHEN** a changed entity has callers but none of them is a test entity
- **THEN** the pure classifier reports that entity as untested and the risk band
  reflects it

#### Scenario: A test caller marks coverage

- **WHEN** a changed entity is referenced by an entity whose kind is a test
- **THEN** the classifier marks that entity covered and does not count it as
  untested

#### Scenario: Risk is deterministic

- **WHEN** the same set of changed entities and blast-radius is scored twice
- **THEN** the pure `risk_score` returns the identical `low`/`medium`/`high` band

### Requirement: The semantic footer reports the blast-radius when a graph exists

thegn SHALL enrich the `◈ semantic` diff footer with the blast-radius summary
when an entity graph is available for the diff's language, reporting the changed
count, callers and distinct files, untested count, and risk band (for example
"3 changed · 14 callers/6 files · 2 untested · risk:high"). The footer MUST
continue to be produced on the hydration thread and delivered over the existing
hydration channel, and its update MUST mark the chrome dirty channel (never a new
wake path or damage channel).

#### Scenario: Enriched footer on a served diff

- **WHEN** a diff in an LSP-served language is hydrated and a graph is present
- **THEN** the footer shows the changed/callers/files/untested/risk summary

#### Scenario: Footer update flows through the existing chrome path

- **WHEN** the blast-radius summary changes
- **THEN** the hydration producer pulses the `TerminalWaker` and the drain marks
  the chrome dirty channel, yielding a `Full` frame with no new tick added

### Requirement: A blast_radius MCP house tool exposes the graph to review agents

thegn SHALL expose the blast-radius as a `semantic.blast_radius`
capability-catalog row projected as a read-scope tool on thegn's MCP server
(`thegn mcp serve`), taking a worktree argument, returning the changed
entities with their callers, the untested set, and the risk band in a
serializable form any external consumer can use. The row MUST be gated by
`required_scope` and hidden when the serving scopes exclude reads. When no
graph is available for the worktree the tool MUST return a "graph
unavailable" result, per the degradation requirement.

#### Scenario: The tool returns the blast-radius for the connection worktree

- **WHEN** an MCP client calls `blast_radius` without an explicit worktree
- **THEN** the router uses the server connection's working directory and
  returns the changed entities, their callers, the untested set, and risk band

#### Scenario: The tool returns the blast-radius for the named worktree

- **WHEN** an MCP client with the read scope calls the blast-radius tool
  with a worktree argument
- **THEN** it receives the changed entities, their callers, the untested
  set, and the risk band for that worktree

#### Scenario: No graph yields a clear unavailable result

- **WHEN** the tool is called for a worktree with no persisted graph
- **THEN** the result states the graph is unavailable instead of erroring or
  returning fabricated emptiness

### Requirement: The blast-radius degrades gracefully without an LSP

thegn SHALL treat the blast-radius as strictly additive: when `[lsp]` is
disabled, no server is available for the diff's language, or the language is
unrecognized, the subsystem MUST write no edges and every surface MUST fall back
without error — the footer to today's intra-diff summary, the `blast_radius` MCP
tool to a "graph unavailable" result, and the review signal to absent. The
AI-free shell MUST NOT hard-depend on the graph or the language server.

#### Scenario: LSP disabled falls back to the intra-diff footer

- **WHEN** `[lsp].enabled` is false and a diff is hydrated
- **THEN** no edges are queried and the footer renders exactly today's
  intra-diff entity summary

#### Scenario: Unserved language yields no blast-radius

- **WHEN** a diff is in a language with no running server or no `Lang` mapping
- **THEN** the `BlastRadius` is absent, the footer degrades, and the MCP tool
  reports the graph is unavailable

### Requirement: A worktree-wide entity index is maintained without a language server

thegn SHALL extend the persistent entity store beyond diff-changed entities
to a worktree-wide index of every tree-sitter-served file, built by an
initial crawl on first worktree open and kept fresh incrementally by the
existing fs-watcher and `source_hash` skip. The crawl and every refresh
MUST run off the event loop, MUST walk the git file listing (never raw
directory recursion into ignored or out-of-root trees), and MUST be capped
by configuration so an oversized worktree yields an honestly partial index
rather than unbounded work. The index MUST NOT require a language server:
parsing is tree-sitter, and edges remain LSP-sourced and optional.

#### Scenario: First open populates the index off the loop

- **WHEN** a worktree in a tree-sitter-served language is opened for the
  first time with `[semantic] worktree_index` enabled
- **THEN** a background crawl parses its git-listed files and writes entity
  rows for all of them, without blocking the event loop

#### Scenario: An oversized worktree degrades to a partial index

- **WHEN** a worktree's file count exceeds `index_max_files`
- **THEN** the crawl stops at the cap, the index is marked partial, and
  every reader reports the partial state instead of presenting it as complete

#### Scenario: Edits refresh only changed files

- **WHEN** the fs-watcher reports changed paths in an indexed worktree
- **THEN** only files whose `source_hash` differs are re-parsed, and
  entities that vanished from a re-parsed file are removed

### Requirement: The repo map is rendered by pure ranked logic

thegn SHALL render a worktree repo map — a ranked, line-budgeted outline of
the indexed entities grouped by file — from pure functions in thegn-core
that take owned rows and perform no I/O, unit-tested to the core coverage
gate. Ranking MUST use caller in-degree from the edge table when edges
exist and MUST fall back to a deterministic structural order (entity-kind
weight, then file path, then line) when they do not; the same input MUST
always produce the identical map. The renderer MUST honor a line budget,
emitting the most important entries first and eliding beyond it.

#### Scenario: Edges rank the map

- **WHEN** the edge table records callers for indexed entities
- **THEN** entities with higher caller in-degree appear before lower-degree
  ones within the budget

#### Scenario: An edge-less index still maps deterministically

- **WHEN** a worktree has entity rows but no edges (no LSP has ever run)
- **THEN** the map renders in the structural fallback order and rendering
  the same rows twice yields the identical output

#### Scenario: The budget bounds the output

- **WHEN** the indexed entities would exceed the line budget
- **THEN** the map stops at the budget with an elision marker rather than
  emitting the full listing

### Requirement: The repo map is available from the CLI

thegn SHALL provide a `map` CLI verb rendering the repo map for the current
or a named worktree, honoring the configured or flag-supplied line budget,
narrowing to a single file's outline on request, and emitting JSON through
the shared machine-readable emitter. When the index is empty and no
compositor owns the worktree, the verb MUST build a capped index inline;
when the worktree has no tree-sitter-served files it MUST say so clearly
rather than printing an empty map.

#### Scenario: A ranked map for the current worktree

- **WHEN** `thegn map` runs inside an indexed worktree
- **THEN** it prints the ranked, budgeted map grouped by file

#### Scenario: JSON output for scripts and agents

- **WHEN** `thegn map --json` runs
- **THEN** the map rows (kind, name, file, line, rank signal) are emitted as
  JSON through the shared emitter

#### Scenario: Headless first use builds the index

- **WHEN** `thegn map` runs in a worktree whose index is empty and no
  compositor is running
- **THEN** the verb crawls up to the cap inline and renders from the result

### Requirement: The repo map is a catalog-projected MCP tool

thegn SHALL expose the repo map as a `semantic.map` capability-catalog row
projected as a read-scope tool on thegn's MCP server, taking worktree and
budget (or single-file) arguments. The row MUST claim exactly the surfaces
that implement it, MUST be gated by `required_scope` like every catalog
verb (never a second policy table), and MUST be hidden when the serving
scopes exclude reads. When the index is unavailable the tool MUST return a
clear "index unavailable" result rather than an error or an empty fabrication.

#### Scenario: An MCP client reads the map

- **WHEN** an MCP client with the read scope calls the map tool with a
  worktree argument
- **THEN** it receives the ranked map rows for that worktree

#### Scenario: Scope gating hides the tool

- **WHEN** `thegn mcp serve` runs with scopes excluding reads
- **THEN** the map tool is absent from the tool listing and calls to it fail

### Requirement: Symbol search falls back to the entity index

Search Everywhere's symbol mode SHALL consult the worktree entity index for
tree-sitter-served languages when the language server yields no answer,
ahead of the regex sweep, so symbol navigation works on LSP-less hosts. The
lookup MUST run off the event loop like the existing symbol workers.

#### Scenario: Symbols resolve without a language server

- **WHEN** symbol mode queries a name in an indexed worktree with `[lsp]`
  disabled
- **THEN** matching entities from the index are returned before any regex
  sweep results
