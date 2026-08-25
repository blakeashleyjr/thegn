# Semantic Graph

## ADDED Requirements

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

## MODIFIED Requirements

### Requirement: A blast_radius MCP house tool exposes the graph to review agents

thegn SHALL expose the blast-radius as a `semantic.blast_radius`
capability-catalog row projected as a read-scope tool on thegn's MCP server
(`thegn mcp serve`), taking a worktree argument, returning the changed
entities with their callers, the untested set, and the risk band in a
serializable form any external consumer can use. The row MUST be gated by
`required_scope` and hidden when the serving scopes exclude reads. When no
graph is available for the worktree the tool MUST return a "graph
unavailable" result, per the degradation requirement.

#### Scenario: The tool returns the blast-radius for the named worktree

- **WHEN** an MCP client with the read scope calls the blast-radius tool
  with a worktree argument
- **THEN** it receives the changed entities, their callers, the untested
  set, and the risk band for that worktree

#### Scenario: No graph yields a clear unavailable result

- **WHEN** the tool is called for a worktree with no persisted graph
- **THEN** the result states the graph is unavailable instead of erroring or
  returning fabricated emptiness
