# Semantic Graph

## ADDED Requirements

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

thegn SHALL expose a `blast_radius` house tool alongside the existing
`semantic_diff` tool in the MCP router, scoped (no args) to the connection's
worktree, returning the changed entities with their callers, the untested set,
and the risk band. The risk band MUST also be available as a serializable signal
the review-gate / change-explanation pipeline can consume.

#### Scenario: The tool returns the blast-radius for the connection worktree

- **WHEN** an MCP client calls `blast_radius`
- **THEN** the router dispatches against the connection worktree and returns the
  changed entities, their callers, the untested set, and the risk band

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
