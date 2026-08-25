# Generic LSP registry — arbitrary language servers slot in

Linear: THE-28

## Why

The LSP substrate (roadmap AQ 529, currently `[~]`) works, but it is closed
over six languages. `thegn_core::semantic::Lang` is a six-variant enum (Rust,
TypeScript, Tsx, JavaScript, Python, Go); `thegn_svc::lsp::resolve_server`
resolves only those variants; the `[[lsp.servers]]` config table is an
**override** mechanism for those same six keys, not a registry — a user who
wants `zls` for Zig, `clangd` for C++, `solargraph` for Ruby, or an in-house
server for a DSL has no path at all. THE-28 asks that thegn "ensure we can add
arbitrary LSPs".

Three adjacent debts surface while touching this seam, found in the audit for
this change:

- **Capability negotiation is absent.** `LspClient::initialize` discards the
  server's `initialize` result, so thegn sends `documentSymbol` / `hover` /
  `signatureHelp` / `codeAction` / `workspace/symbol` requests blind. Servers
  that don't implement a method answer with an error (or worse, stall); with
  arbitrary servers this stops being a corner case.
- **No doctor probe.** `thegn doctor` says nothing about which language
  servers resolve — against the house rule that every backend seam carries a
  Probe.
- **Servers escape the resource ceilings.** LSP servers are spawned straight
  from the host and never join the shared `thegn.slice`
  (`[sandbox.limits]`) — rust-analyzer alone can take gigabytes, and it is
  precisely the kind of background hog the slice exists to bound.

## What Changes

- **`[[lsp.servers]]` becomes a full registry.** An entry declares a language
  key (any string), the file `extensions` it serves, the `language_id` sent in
  `didOpen`, and the server `command`/`args`. The six built-ins remain as
  defaults expressed in the same shape; existing override entries (known
  `lang`, no new fields) keep exactly today's semantics, including
  `command = ""` to disable a language.
- **Supervisor keying moves from the `Lang` enum to the registry key.**
  Per-worktree, lazy, warm lifecycle is unchanged — `(worktree_root,
lang_key)` instances, spawned and initialized off the event loop, failure
  cached. `Lang` survives as the tree-sitter tier (parsing/entity extraction
  stays six-language).
- **Capability negotiation.** The `initialize` result's `capabilities` are
  retained per client; a request whose capability the server did not declare
  is not sent, and the consumer takes its documented fallback instead.
- **A degradation matrix is specified per language tier**: tree-sitter+LSP
  languages get everything (outline fallback, semantic graph, diagnostics,
  hover, symbols); LSP-only registry languages get the LSP-backed features
  with no tree-sitter fallback and no semantic-graph edges; unregistered
  languages keep today's regex fallbacks.
- **`thegn doctor` gains an LSP section** reporting each configured/built-in
  server: enabled state, resolved binary or missing, and the languages/
  extensions it claims.
- **Server spawns join the shared resource slice** via the existing
  fail-safe background wrap.
- The **bridged transport stays a first-class seam**: `LspClient::from_io`
  (in-sandbox / remote servers over bridged stdio) applies to registry
  entries the same as to built-ins.

## Impact

- **Roadmap**: completes **AQ 529** (LSP client substrate — currently `[~]`);
  strengthens the consumers **AQ 519** (Problems), **AQ 523** (Search
  Everywhere symbols), **AQ 530/531/532** (navigation, symbols, hover).
- **Specs**: new capability **`lsp`** (the substrate has real behaviour today
  but no spec of its own — its behaviour is only implied by `semantic-graph`).
  `semantic-graph` is intentionally **not** modified: graph edges remain
  sourced from LSP references for tree-sitter-served languages only.
- **Code (indicative)**: `thegn-core/src/config.rs` (`LspServerConfig`
  gains optional `extensions`/`language_id`), `thegn-svc/src/lsp/mod.rs`
  (registry resolution, negotiation state), `thegn-host/src/lsp.rs`
  (supervisor rekey, slice wrap), consumer sweep in
  `search_everywhere.rs`, `hover.rs`, `panel/sections/symbols.rs`,
  `blast_radius.rs`, `cmd/doctor.rs`.
- **Config**: extended `[[lsp.servers]]` keys documented in
  `config/config.toml.example`; the generated config-reference help page picks
  them up automatically; `docs/help/configuration.md` prose updated.
- **In-flight changes**: `add-config-trust-resolution` — registry commands are
  subprocess argv sourced from config, so they MUST resolve only from trusted
  config layers (see Security in design.md). No overlap with the MCP
  write-tools branch or the Windows programme.
- **No DB schema change. No new external door** (no catalog row: the registry
  is config + in-process consumers, nothing externally invokable).

## Non-goals

- Growing the tree-sitter tier (new grammars) — separate concern, separate
  dependency footprint.
- Multi-root / monorepo workspace folders beyond the current
  one-root-per-worktree model.
- didChange/incremental document sync (thegn opens documents read-only from
  disk today; that stance is unchanged).
- Semantic-graph edges for non-tree-sitter languages (reference mapping needs
  entity spans, which need a parser).
