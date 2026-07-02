# Tasks

## 1. Frecency ranker (superzej-core)

- [ ] 1.1 `frecency.rs`: pure `score(count, last_used_ms, now_ms) -> f64` with a
      bounded half-life decay, and a `rank(entries) -> Vec` helper — **unit tests**:
      more-recent wins at equal count, higher count wins at equal age, zero/negative
      delta does not panic, stable order on ties.
- [ ] 1.2 A cwd → worktree-root resolver reusing the existing `rev-parse
--show-toplevel` path — **unit tests**: nested cwd resolves to root, cwd
      outside any worktree returns none.

## 2. Layout importer (superzej-core)

- [ ] 2.1 `layout_import.rs`: parse a tmuxinator/sesh project file into
      `ImportedLayout { name, root, windows }` — **unit tests**: valid tmuxinator
      project, valid `sesh.toml` `[[session]]`, missing optional fields defaulted,
      malformed input returns an error (no panic).

## 3. Palette + actions (superzej-host)

- [ ] 3.1 Add a repo/worktree `PaletteMode` in `search_everywhere.rs`: list
      workspaces + worktrees ranked by the frecency score, nucleo-filtered; select
      switches the worktree tab and bumps the frecency row.
- [ ] 3.2 "Connect to root" palette/keybind action: read the focused pane's cwd
      (`pane_cwds`), resolve the worktree root, switch to that tab (offer add when
      outside any workspace) — **render test**: the switch is a chrome repaint.
- [ ] 3.3 "Clone and open" palette action: prompt URL, clone off-loop
      (spawn_blocking + channel + `TerminalWaker`), register workspace via the
      add-repo path, open first worktree tab.
- [ ] 3.4 Surface imported layouts as a worktree-template/layout source in the
      new-worktree flow.

## 4. Docs + validate

- [ ] 4.1 Document the frecency opener, connect-to-root, clone-and-open, and the
      importer in `config/config.toml.example` + the palette/navigation doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
