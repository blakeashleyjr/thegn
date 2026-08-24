# Design — workspace search & replace

## Embed vs integrate (the audit's verdict)

| Tool                            | What it offers                                                         | Decision                                                                                                                                                                                                                                                                                         |
| ------------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| scooter                         | TUI find/replace: toggling, preview, changed-line safety, `--no-tui`   | **UX reference only.** Engine + UI are rebuilt on thegn's substrate; an embedded TUI subprocess would break the layer/keymap/help/theming integration and the palette's no-external-process rule. Its safety mechanism (skip a match whose line changed since search) is adopted.                |
| fff-search (in-tree)            | warm index, SIMD grep, `GrepMode::{PlainText,Regex}`                   | **Embed (already embedded).** The search tiers reuse `fff_backend::content_search`/`regex_grep`; pagination bounds honored.                                                                                                                                                                      |
| ast-grep                        | AST-pattern search + rewrite, tree-sitter based, JSON CLI, Rust crates | **Integrate as an external CLI behind a seam.** Embedding the crates drags a second copy of many grammars into every build; the CLI with `--json` is stable, and the seam keeps the vendor inside one impl file. Rewrites are _computed_ by ast-grep but _applied_ by thegn's single write path. |
| skim / television / StringZilla | fuzzy matchers / channel UX / SIMD strings                             | Covered by fff/neo_frizbee; television's channel model belongs to palette plugins (M 168). Not used.                                                                                                                                                                                             |
| SeekStorm / reflex              | full-text / trigram index engines                                      | Out of scope (see proposal non-goals); if a persistent index tier is ever wanted it arrives as another `StructuralSearch`-style seam kind, additive.                                                                                                                                             |
| octocode                        | embedding/LLM code graph                                               | Excised territory; `semantic-graph` (LSP references) owns code intelligence.                                                                                                                                                                                                                     |
| doxx / hygg                     | terminal docx/document viewers                                         | Shape adopted as preview _routes_ (extraction off-loop), not subprocess viewers; the drawer can still be configured with them as file tools.                                                                                                                                                     |

## Where it lives

A **dedicated overlay surface** on the layer compositing path (the
palette/`SearchOverlay` pattern), not a panel section: panel sections are
hydration-fed lists, while this is a modal workflow — two input fields,
options, a selectable result tree, and an apply step with its own keymap.
The overlay holds its state (query, options, results, toggles) for the
session, so reopening resumes where the user left off; nothing persists to
the DB. Per-file "open in editor at match" is an editor-seam handoff.

- **Help context**: the surface registers a help context key
  (`zone:search-replace`) mapped to `docs/help/search-replace.md`; the new
  action ids are claimed there (help + prose ratchets).
- **Damage**: opening/closing/navigating the overlay is chrome ⇒ `Full`
  frames via the existing layer path; streamed result batches arrive on the
  drain and mark the chrome dirty. No new damage channel, no tick.

## Engine and data flow

```text
core (pure, 95% gate)                      host (I/O)
SearchSpec {query, mode, case, word,       overlay UI (fields/tree/preview)
  globs, hidden, ignored, structural}      spawn_blocking search worker:
Match {path, line, span, before,             fff grep / ignore walk / sg --json
  content_hash}                            batches → channel → waker
render_replacement(match, template)        apply worker: read, verify hash,
ReplacePlan / skip-if-changed decision       write temp + rename, report
ApplyReport {applied, skipped, errors}     CLI verb (headless mode)
sg JSON → Match parsing
```

- **Streaming**: the worker sends bounded batches over an unbounded channel
  and pulses the `TerminalWaker` per batch (the `search_everywhere` /
  hydration pattern); the loop drains on wake. Result count is capped
  (`[search] max_results`) with an explicit "truncated" indicator.
- **Cancellation**: every query/option edit bumps a generation token carried
  by the worker; stale batches are discarded at the drain and the worker
  checks the generation between directory batches so an abandoned search
  stops doing work. Closing the overlay cancels the in-flight generation.
- **Replace preview**: rendered per visible match from the pure
  `render_replacement` (regex capture groups `$1…` expanded; literal mode
  verbatim) — no filesystem touch until apply.
- **Apply**: off-thread; per file it re-reads, verifies each selected match's
  recorded content-hash/span still matches (skip + report on drift), applies
  edits bottom-up, writes temp-then-rename in the same directory, preserves
  permissions. The batch never stops on one file's failure; the report lists
  applied/skipped/failed per file and surfaces via the overlay and
  `model.status` (a user-invoked action's failure is never swallowed).

## Structural seam

`StructuralSearch` (sync trait — subprocess-bound): `id()`, `caps()`
(`search`, `rewrite`), `search(spec) -> Vec<Match>`, `rewrite(spec) ->
Vec<Match-with-replacement>`; error type implements `SeamError`
(`NotInstalled` when the binary is absent — the overlay's structural mode
shows why and the textual tiers are unaffected). Kind enum
`[search] structural`: `ast-grep` (default, implemented), `none`, others
reserved. The impl invokes `ast-grep`/`sg` argv-only (pattern, `--lang`,
`--json`), parses JSON in core-tested pure code, and never passes
`--update-all` — thegn's apply path is the only writer. Probe: binary +
version, offline.

## Capability catalog

Two rows, projected per the one-catalog rule with `required_scope(verb)`:

- `search.query` — read scope; surfaces: CLI (`thegn search <pattern>`
  with mode/glob flags, JSON output for scripting), and control/MCP where the
  read surface already projects.
- `search.replace` — write scope; CLI `thegn search --replace <tpl> --apply`
  (headless, scooter's `--no-tui` analogue; without `--apply` it prints the
  plan). The MCP projection depends on the in-flight write-MCP scope-gating
  work and is listed as a gap until that lands — no second policy table.

## Security

- **Blast radius**: replace is a bulk write surface over the worktree. It is
  contained to the invoking worktree root: the walker never follows symlinks
  out of the root, `.git/` is always excluded from both search and apply, and
  apply refuses paths that resolve outside the root. Read-only worktrees (the
  canonical checkout is mounted RO) produce per-file errors, not a crash or a
  partial silent write.
- **Scope gating**: `search.replace` requires the write scope via
  `required_scope`; the headless CLI apply honors the same verb. No
  credentials involved anywhere; no tokens in config.
- **Subprocess**: ast-grep runs argv-only (no shell), cwd = worktree root,
  wrapped by the file-tools memory-cap containment (the existing
  file-explorer requirement), and its output is parsed defensively (bounded
  JSON, malformed ⇒ tier error, not a crash). Patterns are user input passed
  as a single argv element.
- **Integrity**: the changed-since-scan hash check prevents clobbering
  concurrent edits (agents run in these worktrees); temp-then-rename prevents
  torn files on crash.

## Preview routes (non-text)

Three additive routes on the viewer seam from `add-viewers-and-quick-open`
(same off-loop parse + channel + waker discipline, same graceful text
fallback): `.docx` → extracted text with headings/tables (doxx-style, via a
pure-Rust extraction off-thread); archives (`.zip`/`.tar*`) → entry listing
(name/size, bounded); unknown binary → the existing hex view instead of raw
bytes. No new render substrate.

## Persistence

None. No SQLite schema change, no `user_version` bump; overlay state is
in-memory for the session.

## Open questions

- Should apply write an undo journal (per-file pre-image under state dir)
  for one-shot revert? Leaning yes-later: the hash-skip plus git worktree
  status already give recovery; an undo step is additive.
- Should the structural tier surface language auto-detection or require an
  explicit `--lang` in the overlay? Start explicit (ast-grep's own
  extension-map default), revisit with usage.
- Multiline textual search (scooter's `-U`): the fff grep tier is line-based;
  multiline could route through the regex tier reading whole files. Deferred
  unless demanded; the structural tier covers most multiline intents.
