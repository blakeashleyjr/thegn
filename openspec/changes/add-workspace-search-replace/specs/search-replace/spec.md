# Search & Replace

## ADDED Requirements

### Requirement: Workspace search runs off the event loop, streamed and cancellable

Workspace content search SHALL run on a background worker (never on the event
loop), stream results in bounded batches over a channel with a
`TerminalWaker` pulse per batch, and be cancellable: every query or option
edit MUST supersede the previous search via a generation token, stale batches
MUST be discarded at the drain, and the worker MUST observe cancellation
between batches so an abandoned search stops consuming CPU. The result set
MUST be bounded by `[search] max_results` with an explicit truncation
indicator, and an idle open surface MUST cost nothing (no polling, no ticks).

#### Scenario: Results stream while the user types

- **WHEN** a query is edited mid-search
- **THEN** the previous generation's in-flight batches are discarded, its
  worker stops at the next batch boundary, and only the new generation's
  results render

#### Scenario: Search never blocks the loop

- **WHEN** a search over a large worktree is running
- **THEN** the loop keeps compositing frames, results arrive via channel
  drains on waker pulses, and an idle wake still yields a `Skip` render plan

### Requirement: A dedicated surface previews and selectively applies replacements

thegn SHALL provide a focusable Search & Replace surface with query and
replacement fields, literal and regex modes (regex capture groups expandable
in the replacement), case/whole-word options, glob include/exclude, and
hidden/ignored toggles honoring `[search] respect_gitignore`. Results MUST be
grouped by file with per-match and per-file toggles, each visible match MUST
show a before/after preview rendered without touching the filesystem, an
invalid regex MUST surface as an inline error without starting a search, and
a match row MUST offer an editor-seam handoff opening the file at that line.
Surface state persists for the session only — no database write.

#### Scenario: A toggled-off match is not applied

- **WHEN** the user deselects one match in a file and applies
- **THEN** every selected match is replaced and the deselected span is left
  byte-identical

#### Scenario: Capture groups render in the preview

- **WHEN** mode is regex with query `fn (\w+)` and replacement `fn $1_v2`
- **THEN** each match's preview shows the captured name substituted before
  any apply happens

### Requirement: Replacements apply through one guarded write path

All replacement writes — from the surface, the CLI, and the structural tier —
SHALL go through a single apply path that runs off the event loop and, per
file: re-reads the file, verifies each selected match's recorded content
snapshot still holds (a drifted match MUST be skipped and reported, never
applied against changed content), applies edits bottom-up, and writes
atomically (temp-then-rename in place, permissions preserved). The path MUST
confine writes to the worktree root (no symlink escape) and MUST always
exclude `.git/`. One file's failure (read-only, permission denied) MUST be
reported per-file without aborting the batch, and the apply summary MUST be
surfaced — never silently swallowed.

#### Scenario: A file changed since the scan is skipped

- **WHEN** a matched line was modified between search and apply
- **THEN** that match is skipped, the report names the file as drifted, and
  the other files' replacements still apply

#### Scenario: A read-only worktree reports instead of aborting

- **WHEN** apply hits a file it cannot write
- **THEN** that file is reported failed with the reason and the remaining
  files are still processed

### Requirement: Structural search is an optional provider seam

Structural (AST-pattern) search and rewrite SHALL be a provider seam: an
object-safe trait with a `[search] structural` config kind (`ast-grep`
implemented, `none`, others reserved), caps declaring search and rewrite
support, a `SeamError`-classified error type, and a cheap offline `Probe` in
`thegn doctor`. The ast-grep implementation MUST invoke the vendor CLI
argv-only from its implementation file, fold JSON results into the same match
model, and MUST NOT let the vendor tool write files — structural rewrites
apply only through the guarded write path. An absent binary MUST degrade as
`NotInstalled`: the structural mode explains what is missing while the
textual tiers keep working.

#### Scenario: Structural rewrite goes through the guarded path

- **WHEN** an ast-grep pattern with a rewrite is applied
- **THEN** ast-grep only computes matches and replacement text, and the
  guarded apply path performs every write with its drift and atomicity rules

#### Scenario: Missing ast-grep degrades to textual search

- **WHEN** `[search] structural = "ast-grep"` but the binary is not on PATH
- **THEN** structural mode reports the missing binary, literal/regex search
  is unaffected, and `thegn doctor` shows the probe as unavailable

### Requirement: Search and replace project the capability catalog

Externally invokable search operations SHALL be `thegn_core::capability::CATALOG`
rows gated by `required_scope(verb)`: `search.query` (read scope; the
`thegn search` CLI verb with JSON output) and `search.replace` (write scope;
`thegn search --replace` prints a plan, `--apply` performs it through the
guarded write path). No surface may carry a second policy table; the MCP
projection of `search.replace` follows the write-MCP scope-gating work and is
recorded as a listed gap until it lands.

#### Scenario: Headless replace without apply is a dry run

- **WHEN** `thegn search --replace <tpl>` runs without `--apply`
- **THEN** the plan (files, match counts, previews) prints and no file is
  modified

#### Scenario: Apply requires the write scope

- **WHEN** a caller holding only the read scope invokes the
  `search.replace` verb
- **THEN** the operation is refused by the scope check shared with every
  other catalog surface
