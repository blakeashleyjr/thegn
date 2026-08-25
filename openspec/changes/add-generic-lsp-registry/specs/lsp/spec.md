# LSP

## ADDED Requirements

### Requirement: Arbitrary language servers are declared in one registry

thegn SHALL treat `[[lsp.servers]]` as a full server registry, not an
override table: an entry MAY declare any language key (`lang`), the file
`extensions` it serves, the `language_id` sent in `textDocument/didOpen`
(defaulting to the key), and the server `command`/`args`. The built-in
servers (rust, typescript, tsx, javascript, python, go) MUST be expressed as
registry entries in the same shape, and a user entry with a built-in key
MUST override the built-in field-wise with today's semantics preserved: an
override command is used as given, a built-in default command is used only
when its binary is on PATH, and `command = ""` disables the language. A
non-built-in entry with no `extensions` MUST be rejected by config
validation.

#### Scenario: A non-built-in server is registered

- **WHEN** config declares `[[lsp.servers]]` with `lang = "zig"`,
  `extensions = ["zig", "zon"]`, `command = "zls"`
- **THEN** opening a `.zig` file resolves the `zig` registry entry and the
  LSP consumers (diagnostics, symbols, hover) use `zls` for it

#### Scenario: Existing override entries keep their semantics

- **WHEN** config declares `[[lsp.servers]]` with only
  `lang = "rust"`, `command = "my-ra"` (no new fields)
- **THEN** Rust files use `my-ra` with the built-in extensions and
  language id, exactly as the override behaved before the registry

#### Scenario: Disabling a language still works

- **WHEN** an entry sets `command = ""` for a key
- **THEN** no server is resolved for that key and its consumers take their
  documented fallbacks

### Requirement: Files resolve to servers by extension

thegn SHALL resolve a file to a registry entry by its extension, and the
resolver MUST be pure logic (unit-testable without I/O). When two entries
claim the same extension, config validation MUST flag the collision naming
both entries, and at runtime the first-declared entry MUST win so a bad
config degrades instead of failing.

#### Scenario: Extension resolves to the declaring entry

- **WHEN** a registry entry declares `extensions = ["rb"]`
- **THEN** `lib/foo.rb` resolves to that entry's key

#### Scenario: A collision is flagged but does not break resolution

- **WHEN** two entries both claim `ext = "x"`
- **THEN** `thegn config validate` reports the collision naming both keys,
  and at runtime files resolve to the first-declared entry

### Requirement: Server lifecycle is lazy, warm, and per-worktree

thegn SHALL keep one server instance per `(worktree root, registry key)`.
A server MUST NOT be started eagerly: the first request for a key spawns
and initializes it off the event loop, and the instance stays warm across
tab switches until shutdown. A spawn or initialize failure MUST be cached
per key so a missing server is not respawned on every request, and clients
MUST be shut down when the supervisor is dropped. The bridged stdio
transport (in-sandbox / remote servers) MUST apply to registry entries the
same as to built-ins.

#### Scenario: First use spawns off the loop

- **WHEN** the first symbols request for a worktree's Zig file arrives
- **THEN** the `zig` server is spawned and initialized off the event loop,
  and subsequent requests for that `(root, key)` reuse the warm client

#### Scenario: A missing server is cached, not respawned

- **WHEN** a registry entry's command is not found at spawn
- **THEN** the failure is cached for that `(root, key)` and later requests
  fail fast to their fallbacks without a new spawn attempt

### Requirement: Requests are gated by negotiated server capabilities

thegn SHALL retain the `capabilities` object from each server's
`initialize` result and MUST NOT send a request whose corresponding
provider capability the server did not declare; the request instead fails
with the same not-available error a missing server produces, flowing into
the consumer's documented fallback. The capability check MUST be pure
logic tolerating the boolean-or-object union the LSP spec allows, and an
absent or malformed capabilities object MUST gate every optional method
off rather than sending blind.

#### Scenario: An undeclared method is never sent

- **WHEN** a server's `initialize` result declares no `hoverProvider`
- **THEN** hover requests for its files return not-available without
  touching the wire, and the hover popup falls back exactly as if no
  server were running

#### Scenario: A declared capability object counts as support

- **WHEN** a server declares `documentSymbolProvider` as an options object
  rather than `true`
- **THEN** the gate treats document symbols as supported and the request is
  sent

### Requirement: Features degrade by language tier

thegn SHALL degrade per language tier: a language with both a tree-sitter
grammar and a server gets LSP-backed features with tree-sitter fallbacks
and semantic-graph edges; a registry-only language gets LSP-backed
features with no tree-sitter fallback and MUST NOT get semantic-graph
edges (entity spans require a parser); an unregistered language keeps the
non-LSP fallbacks. No tier failure may surface as an error on the primary
path — absence degrades silently to the next provider.

#### Scenario: Registry-only language outline

- **WHEN** a registry-only language's server answers `documentSymbol`
- **THEN** the Symbols section shows the server's outline; if the server is
  missing or gated, the section shows its empty state rather than a wrong
  regex parse

#### Scenario: Graph edges stay tree-sitter-tier

- **WHEN** the semantic-graph builder processes a diff in a registry-only
  language
- **THEN** no edges are written for it and the blast-radius surfaces degrade
  per the semantic-graph capability

### Requirement: `thegn doctor` reports the LSP registry

`thegn doctor` SHALL include an LSP section listing every registry entry —
built-in and user-declared — with its key, the extensions it claims, the
resolved command or `missing`, and whether `[lsp].enabled` or a
`command = ""` entry masks it. The probe MUST be existence-only: doctor
MUST NOT spawn a server.

#### Scenario: Doctor lists resolution state

- **WHEN** `thegn doctor` runs with one built-in resolvable, one built-in
  missing from PATH, and one user entry
- **THEN** the LSP section shows all three with their keys, extensions, and
  resolved-or-missing state, and no server process is started

### Requirement: Local servers join the shared resource ceilings

thegn SHALL wrap locally spawned language-server argv with the shared
background resource wrap so servers run inside `thegn.slice` under
`[sandbox.limits]`. The wrap MUST be fail-safe: an unpublished slice
policy or unusable `systemd-run` runs the server exactly as before.
Bridged (`from_io`) servers are bounded by their sandbox and MUST NOT be
wrapped by the host.

#### Scenario: A hungry server is bounded by the slice

- **WHEN** the slice policy is published and a local server is spawned
- **THEN** its process runs inside `thegn.slice` and counts against
  `cpu_total`/`memory_total` alongside panes and background jobs

#### Scenario: The wrap never breaks the server

- **WHEN** the slice policy is unpublished or `systemd-run` is unusable
- **THEN** the server is spawned unwrapped, exactly as today

### Requirement: Registry commands resolve only from trusted config layers

Because a registry entry is subprocess argv that runs on first use, thegn
MUST NOT execute `[[lsp.servers]]` commands sourced from a worktree-local
config layer: until config trust resolution lands, worktree-layer entries
SHALL be ignored with a status-line notice, and once it lands the trust
decision governs.

#### Scenario: A worktree-local entry does not auto-run

- **WHEN** a checked-out worktree carries a config file declaring an
  `[[lsp.servers]]` entry
- **THEN** the entry is ignored, a status notice says why, and no command
  from that file is spawned
