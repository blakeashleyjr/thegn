# Design — generic LSP registry

## What exists today (audited)

- `thegn_svc::lsp` — hand-rolled JSON-RPC client over stdio: framing codec
  (`lsp/framing.rs`), reader thread, id-correlated responses,
  `publishDiagnostics` forwarded on a channel. Transport-abstracted:
  `LspClient::start` (local child) and `LspClient::from_io` (bridged stdio for
  in-sandbox/remote servers). Deliberately minimal hand-rolled protocol types
  with defensive parsing (Location vs LocationLink, DocumentSymbol vs
  SymbolInformation, MarkedString vs MarkupContent) — this stays.
- `thegn_host::lsp::LspSupervisor` — lazy, warm, per-`(worktree_root, Lang)`
  clients; "no server" cached per key; diagnostics bridge thread pulses the
  `TerminalWaker`. `LspDiagnostics` partitions pushed diagnostics by root.
- Consumers: Problems panel (diagnostics), Symbols panel section
  (documentSymbol outline with tree-sitter fallback), hover/signature/
  code-action popup, Search Everywhere symbol mode (LSP-first with regex
  sweep fallback), go-to-def/refs from the Symbols section, and the semantic
  graph builder (`textDocument/references`).
- Closed world: `Lang` (6 variants), `lang_key`, `language_id`,
  `default_command`, and `resolve_server` all switch on the enum;
  `LspServerConfig` carries only `lang`/`command`/`args` and can only
  override or disable one of the six.
- `initialize` result is **discarded** — no capability gating.
- No `thegn doctor` LSP section. Server spawns do not join `thegn.slice`.

## Registry model

Two conceptual layers, one config table:

```toml
[[lsp.servers]]
lang = "zig"                  # any key; the six built-in keys select overrides
extensions = ["zig", "zon"]   # required for a non-built-in key
language_id = "zig"           # didOpen languageId; defaults to `lang`
command = "zls"
args = []
```

- **Built-ins are data, not code paths**: the six current defaults
  (rust-analyzer, typescript-language-server, pyright-langserver, gopls) are
  expressed as registry entries with their extensions and languageIds
  pre-filled; user entries with a matching `lang` override them field-wise
  (today's semantics: an override command is trusted outright; the built-in
  default is used only when its binary is on PATH; `command = ""` disables).
- **Resolution is by extension**: file path → extension → registry key. The
  pure resolver lives beside today's `resolve_server` in `thegn-svc` (unit
  tested there; `thegn-core` only carries the config types, keeping core
  substrate-free). Extension collisions are a config-validation error
  (first-declared wins at runtime so a bad config still degrades, but
  `thegn config validate` flags it).
- **`Lang` stays** as the tree-sitter tier. A small adapter
  (`Lang::from_key`) links a registry key to its tree-sitter grammar when one
  exists; consumers that need spans (semantic graph, outline fallback) require
  it, consumers that only need the wire protocol (diagnostics, hover,
  workspace symbols) use the registry key alone.

## Supervisor rekey

`ClientMap` becomes `HashMap<(PathBuf, String), Option<Arc<LspClient>>>` keyed
by registry key. Everything else about the lifecycle is unchanged and is now
**specified**: never started eagerly, first request spawns + initializes
off-loop (`spawn_blocking`), warm across tab switches, `None` cached on
failure so a missing server is not respawned per request, shutdown-on-drop.
Distinct keys sharing one command (ts/tsx/js →
`typescript-language-server`) keep today's one-client-per-key behaviour;
coalescing them into one shared instance is an open question below.

## Capability negotiation

`LspClient::initialize` retains the `initialize` result's `capabilities`
object. A small pure gate — `supports(method, &capabilities) -> bool` —
answers from the declared provider fields (`documentSymbolProvider`,
`hoverProvider`, `referencesProvider`, `definitionProvider`,
`signatureHelpProvider`, `codeActionProvider`, `workspaceSymbolProvider`),
tolerating the bool-or-object union each of these allows. Request methods
check the gate and return `LspError::NotAvailable` without touching the wire,
which flows into each consumer's existing fallback path (the same error a
missing server produces today — no new consumer plumbing). The gate function
is pure and unit-tested in `thegn-svc` against real-world capability shapes.

## Event loop / rendering

No new wake path and no new damage channel. Spawn/initialize stay off-loop;
diagnostics keep the existing bridge-thread → channel → waker delivery and
mark the chrome dirty channel exactly as today (`Full` frames via the
existing drain). Doctor is a CLI verb, off the compositor entirely.

## Resource ceilings

Server spawn argv passes through `sandbox_cpucap::wrap_background_argv` so
servers join `thegn.slice` under `[sandbox.limits]`. Same fail-safe contract
as the fold gate and agent handoff: an unpublished policy or unusable
`systemd-run` runs the server exactly as before — a cap that silently breaks
hover would be worse than no cap. The wrap applies only to the local-child
transport; a bridged (`from_io`) server is bounded by its sandbox, not by the
host slice.

## Doctor probe

A `doctor` section lists every registry entry (built-in and user-declared):
key, extensions, resolved command (PATH lookup or explicit path) or
`missing`, and whether `[lsp].enabled` masks it. Pure formatting over a
resolved list; no server is spawned by doctor (probing is existence-only —
launching arbitrary servers from doctor would be a surprising side effect).

## Alternatives considered

- **A `Lang` enum extension per language** — rejected: every new language
  would be a code change; the issue asks for arbitrary servers.
- **Adopting the `lsp-types` crate** — rejected: the hand-rolled defensive
  parser is a deliberate, documented choice (small footprint, tolerant of
  server quirks); negotiation needs ~7 fields, not the whole protocol.
- **Probing servers at startup** — rejected: violates lazy start and the 0%
  idle stance; doctor + first-use cover discovery.
- **Root markers / per-language project roots** (e.g. `Cargo.toml` upward
  search) — deferred: worktree root is thegn's unit of everything; per-file
  sub-roots complicate the partition model for no demonstrated need.

## Security

- **Registry commands are subprocess argv from config.** They MUST resolve
  only from trusted config layers (user/global — and workspace config only
  per the trust decision in the in-flight `add-config-trust-resolution`); a
  worktree-local file must never inject a command that auto-runs on first
  panel open. Until trust resolution lands, worktree-layer `[[lsp.servers]]`
  entries are ignored with a status-line notice.
- **No secrets**: entries carry argv only; env passthrough is the pane
  environment, no credential material is added. No SecretRef surface needed.
- **Blast radius**: servers read the worktree (that is their job) and run
  with thegn's uid. The slice wrap bounds CPU/memory. A malicious server
  named in trusted config is equivalent to any `[[tools]]` entry — the trust
  boundary is the config file, and that is where it is enforced.
- **Bridged servers** run inside the pane's sandbox and are bounded by it;
  the host only sees framed JSON-RPC.
- **No new external door**: nothing here is remotely invokable; no catalog
  row, no scope.

## Open questions

- Should several registry keys be able to share one server instance
  (`typescript-language-server` serving ts/tsx/js as one process)? Saves
  memory; complicates the cache-key and shutdown story. Deferred — today's
  per-key instances are correct, just not minimal.
- Should the doctor probe optionally do a spawn + initialize + shutdown
  round-trip behind a flag (`thegn doctor --lsp-handshake`)? Useful for
  debugging registry entries; deferred until asked for.
