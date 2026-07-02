# Design

## Frecency score (core, pure)

A pure function in `superzej-core` scores an entry from its `frecency`/`repos`
row: `score(count, last_used, now) = count * decay(now - last_used)` with a
bounded half-life decay (the classic frecency curve). It is **pure + unit-tested**
(more-recent beats older at equal count; higher count beats lower at equal age;
never panics on a zero/negative delta). The `repos` table already carries
`open_count` + `last_opened`; the palette `frecency` table carries generic
`(key, count, last_used)`. The ranker reads these; no new column.

## Palette mode (host)

`search_everywhere.rs` gains a repo/worktree `PaletteMode` (alongside
Files/Content/Git/…). It lists workspaces + their worktrees, ranked by the
frecency score, filtered through the existing nucleo matcher. Selecting an entry
switches to that worktree tab (the existing one-session tab-switch, never a
teleport) and bumps its frecency row. Opening remains a `chrome dirty` repaint +
the normal tab-activation path.

## Connect-to-root (host + core)

A resolver in core takes a cwd and returns the owning worktree root via git
(`rev-parse --show-toplevel` semantics, already used for sidebar labels). A
palette/keybind action "reveal this pane's worktree" reads the focused pane's cwd
(persisted in `pane_cwds`) and switches to the matching worktree tab. If the cwd
is outside any registered workspace, it offers to add it (reusing the add-repo /
add-dir path).

## Clone-and-open (host)

A palette action prompts for a URL, runs the clone **off-loop** (spawn_blocking,
result handed back over the mpsc channel + `TerminalWaker` pulse — never on the
loop), registers the clone as a workspace, and opens its first worktree tab.
Reuses `worktree::add_checked` and the existing add-repo path.

## Layout import (core, pure)

A pure parser reads a `tmuxinator`/`sesh` YAML/TOML project file into a neutral
`ImportedLayout { name, root, windows: [{name, cwd, command}] }`. It is
**unit-tested** (valid tmuxinator project, valid sesh.toml `[[session]]`, missing
optional fields, malformed input → error not panic). The import is offered as a
worktree-template/layout source; it does not mutate the source files.

## Invariants

- **Event loop**: clone/import filesystem work runs off-loop and wakes the loop
  on completion; no polling timer, no blocking I/O on the loop.
- **Render**: opening/switching is a `chrome dirty` repaint (sidebar focus + tab
  activation), not a pane recompose. render_plan invariants unchanged.
- **State**: no `user_version` bump — the `frecency` and `repos` tables already
  exist; ranking is read-time.
- **Additivity**: pure shell navigation; no proxy/agent dependency. The frecency
  ranker and importer live in core with no tokio/termwiz deps.

## Alternatives considered

- **Raw-recency ordering (status quo, `seq DESC`)** — kept as a fallback but
  frecency better matches "the place I actually work in."
- **Shelling out to `zoxide` as the store** — rejected as a hard dep; superzej's
  own table is the source of truth, with optional zoxide enrichment later.
- **A live filesystem crawl for repos** — rejected; the `repos`/workspace registry
  is authoritative and cheap.
