# Workspace-wide search & replace

Linear: THE-5

## Why

thegn can find but not change. The palette's Content mode (`/`) streams
grep hits off-thread through the embedded fff-search engine, but it shows at
most 8 rows, is ephemeral (results die with the palette), and has no replace
anywhere — the one VSCode surface a worktree IDE cannot ship without. The
comparable-tool audit (scooter, television, skim, ast-grep, StringZilla,
SeekStorm, reflex, octocode, doxx, hygg) says the shape is settled: an
interactive search+replace surface with per-match toggling, before/after
preview, and a changed-since-scan safety check (scooter); structural
search/rewrite by AST pattern as a distinct tier (ast-grep); document
previews for non-text formats (doxx). thegn already owns the hard parts —
a warm SIMD grep index (fff-search, plain-text + regex modes), the
`ignore` walker, tree-sitter grammars, streamed off-thread search with
generation-token staleness, and a preview pane with text/graphics routes.

The roadmap's Kyde note excludes _in-buffer_ find/replace (thegn stays a
viewer and hands editing to `$EDITOR`); workspace search & replace is not
that — it is a batch file operation with preview and confirmation, the same
family as the drawer's rename/delete, with per-file editing handed off to the
editor seam.

## What Changes

- **New `search-replace` capability**: a dedicated focusable Search & Replace
  surface (composited on the layer path like the palette/search overlay, with
  its own help context) — query + replacement fields; literal/regex modes with
  capture-group expansion; case/word/glob include-exclude/hidden+ignored
  options; results streamed in batches, grouped by file, with per-match and
  per-file toggles and a before/after preview line; apply with a summary.
- **Embed, not integrate, for the text engine**: search runs on the already
  embedded fff-search grep (plain + regex) and `ignore` walker — no scooter
  subprocess, matching the command-palette rule that content search never
  shells out. Scooter is the UX reference, not a dependency.
- **Structural tier as a provider seam**: a `StructuralSearch` seam (kind
  `ast-grep` implemented as an external-CLI impl invoked argv-only with JSON
  output; other kinds reserved) adds AST-pattern search and rewrite
  computation. ast-grep never writes files: its rewrites are folded into the
  same neutral match model and applied through thegn's single guarded write
  path. Missing binary degrades to the textual tiers (`NotInstalled`), probed
  by `thegn doctor`.
- **One guarded apply path**: replacements apply off the event loop with a
  changed-since-scan skip (content-hash snapshot per match), atomic per-file
  write, `.git` exclusion, worktree-root containment, and per-file error
  reporting (read-only files/worktrees report, never abort the batch).
- **Event-loop contract, spelled out**: search and apply run off-thread,
  stream over a channel with `TerminalWaker` pulses, and are cancelled by
  generation tokens on every query edit; the loop never blocks or polls.
- **Capability catalog rows**: `search.query` (read scope) and
  `search.replace` (write scope) projected as `thegn search` CLI verbs
  (headless `--replace --apply` mode included); the MCP projection of
  `search.replace` depends on the in-flight write-MCP scope-gating work.
- **Palette handoff**: an action (an `ActionSpec`, per the palette contract)
  opens the surface seeded with the palette Content query.
- **Non-text preview routes** (the audit's preview half): `.docx` renders as
  extracted text (headings/tables) through the existing text route, archives
  list entries, and unknown binary previews fall back to the hex view — all
  parsed off-loop on the existing preview seam.

## Non-goals

- **In-buffer editing / editable diff** — stays excluded per the roadmap's
  Kyde note; per-file editing is an editor-seam handoff (open at match line).
- **Embedding a full-text index server** (SeekStorm) or trigram index daemon
  (reflex) — fff-search's warm index already serves interactive latency at
  worktree scale; an index server is a different product tier.
- **Embedding/LLM semantic code search** (octocode) — AI-adjacent, excised
  territory; must never be a shell dependency. Reference-finding and
  blast-radius stay with the LSP-backed `semantic-graph` capability; this
  change is strictly textual/structural.
- **File renaming engines** (nomino) — rename lives in the drawer
  (file-explorer 606).
- **A new matcher** (skim/television/StringZilla) — fff-search/neo_frizbee
  already cover SIMD fuzzy + grep; television's channel model is palette
  territory (M 168), not this change.

## Impact

- **Linear**: THE-5 (robust file search and replace / file management audit).
- **Roadmap**: group **AF** (extends 395–400 file viewer/search; adds the
  search-replace item) and **M 167** (Search Everywhere) for the palette
  handoff; the preview routes extend AF 396/399/400.
- **Specs**: new `search-replace` capability; `file-explorer` ADDED (non-text
  preview routes, extending the document-viewers requirement from
  `add-viewers-and-quick-open`); `command-palette` ADDED (Content-mode
  handoff).
- **Config**: new `[search]` table (`respect_gitignore`, `include_hidden`,
  `max_results`, `structural` kind) — every key documented in
  `config/config.toml.example`.
- **New action ids** (`search-replace-open`, plus surface-internal bindings)
  and a new help context ⇒ a new `docs/help/search-replace.md` claims them
  (help + prose ratchets).
- **Catalog**: two new `CATALOG` rows with `required_scope(verb)` gating —
  never a second policy table.
- **In-flight overlap**: `add-viewers-and-quick-open` owns the viewer seam
  this extends (delta only adds routes); the write-MCP scope-gating branch is
  a dependency for the `search.replace` MCP surface (not re-scoped here);
  `add-file-manager-seam` (THE-14, written alongside) is independent.
- **Core/host split**: match model, replacement rendering, plan/skip
  decisions, and ast-grep JSON parsing are pure `thegn-core` logic under the
  95% gate; walking, subprocess, overlay UI, and apply I/O live in the host
  and are smoke/e2e-covered.
