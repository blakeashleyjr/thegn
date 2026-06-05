# superzej zellij plugins

Four Rust → WASM `zellij-tile` plugins, each a standalone crate (its own
`Cargo.toml`/`Cargo.lock`, with an empty `[workspace]` so the host repo isn't
treated as a workspace root). Build with `wasm32-wasip1`:

```sh
just build-plugins                                                                # all, via cargo
nix build .#superzej-sidebar .#superzej-panel .#superzej-tabbar .#superzej-statusbar  # via the flake
```

They deploy to `~/.local/share/superzej/{sidebar,panel,tabbar,statusbar}.wasm`,
which the session layout (`layouts/superzej.kdl`) pins left (sidebar) and right
(panel) as framed boxes, with the tabbar across the full-width top and the
statusbar along the bottom. zellij `shellexpand`s the
`file:~/…` paths, so no absolute path is baked in. All render with the superzej
theme palette (truecolor, kept in sync with `config/zellij.kdl`).

## `sidebar/` — left repo → branch tree

Everything is one zellij session; each repo is a group of tabs named
`{repo_slug}/{branch}` (its main checkout is `{slug}/home`).

- Renders a one-level tree under a bold **`WORKSPACES`** header. **Open** repos
  come from the live **`TabUpdate`** (grouped by the tab-name prefix) — each repo
  with its worktree branches nested below (`├`/`└`); the active repo gets a cyan
  rail, the active branch is cyan. **Closed** repos (managed but with no open tab)
  are pulled from `superzej workspaces` over `run_command` and shown dimmed with a
  `○` marker — so every managed repo is listed at once.
- Fully interactive: a keyboard cursor (**↑/↓** or **j/k**) and the **mouse
  hover** both highlight a row; **Enter**/click activates it — a repo row
  `switch_tab_to`s its `{slug}/home` tab (opening it first if closed, via
  `superzej new-workspace <path>`), a branch row `switch_tab_to`s that worktree's
  tab, the **+ worktree** row under each open repo adds a worktree to it
  (`superzej new-worktree --repo <path>` — a new `{slug}/{branch}` tab), and the
  trailing **+ new workspace** row opens a slick **fzf** browser over `$HOME`
  (`superzej new-workspace --from-home`) with a live git preview. **Selecting is
  always a tab switch — never a session switch**, so the sidebar/tabbar/panel stay
  put and only the middle terminal + right panel change.
- A `superzej_refresh` pipe re-pulls the repo list (sent right after a repo is
  registered, so a newly-added repo shows up immediately).
- The whole panel lights up (cyan header bar) when it's the focused pane.
- The sidebar does **not** manage its own visibility — the **statusbar** does
  (see below). A suppressed plugin can't reliably re-show _itself_ (nor observe
  the terminal width while suppressed), so the one always-visible, full-width
  surface owns hide/show for both the sidebar and the panel.

## `tabbar/` — full-width top strip

- Replaces zellij's built-in `tab-bar` so there's no "Zellij (session)" wordmark
  and no swap-layout (`BASE`) indicator — just the **active repo's** branch tabs,
  centered in a borderless 1-row strip spanning the full width along the top. It
  sits **above** the three framed boxes (sidebar | center | panel), so every
  box's top border lands on the same row right beneath it. Tabs are named
  `{slug}/{branch}`; the strip filters to the focused tab's `{slug}` and shows
  only the `{branch}` suffix.
- Subscribes to **`TabUpdate`**; the active tab is a filled cyan chip. **Hover**
  highlights a tab; **click** → `switch_tab_to`.

## `panel/` — right diff / PR panel

- Tracks the focused **(session, tab)** via `SessionUpdate`/`TabUpdate`
  (`PaneInfo` carries no cwd), then asks `superzej resolve-worktree` for the path.
- Drives the binary over zellij's `run_command` bridge — `superzej pr status
--json` (cache-served) and `superzej diff --stat` — routed back via
  `RunCommandResult`; also ingests `superzej pr watch` pushes via `pipe()`
  (`superzej_pr`).
- Renders the PR header (number/title/state), CI rollup (✔/✗/⧗), review decision,
  and the diff stat. Action keys run `superzej pr …`: `o` open · `r` rerun ·
  `f` refresh inline; `m` merge · `c` create · `a` approve in a floating pane that
  prompts for confirmation. `Ctrl-Alt-p` toggles its visibility — handled by the
  statusbar controller (below), not the panel itself.

## `statusbar/` — bottom context hint bar + visibility controller

- A single-line bar pinned along the bottom of every tab. Subscribes to
  **`ModeUpdate`** and **`TabUpdate`** and shows only the keys relevant _now_:
  it switches on the input **mode** (Normal / Pane / Tab / Resize / …) and, in
  Normal mode, on whether the focused tab is a repo **home** tab or a
  **worktree** tab.
- Normal mode leads with `Cmd-K menu`, tab/pane navigation, and the common
  superzej actions; other modes show that mode's primary keys plus a mode chip on
  the left. Styled to the theme.
- **Owns sidebar/panel visibility.** It is the one chrome surface that is never
  hidden and always full width, so it can hide/show the others and reapply the
  layout — a suppressed plugin can't reliably re-show _itself_, nor see the
  terminal width while suppressed. It tracks their pane ids from `PaneUpdate`
  and drives two behaviors:
  - **Manual toggle:** `Ctrl-Alt-s` / `Ctrl-Alt-p` pipe `superzej_toggle_{sidebar,panel}`
    here (via `MessagePlugin`, no spawned pane); `superzej {sidebar,panel} --toggle`
    is the CLI/menu path. State is persisted to `~/.superzej/.{sidebar,panel}_state`
    so new tabs load consistent. CLI pipes are unblocked immediately and the
    payload-less EOF dupe ignored.
  - **Narrow-terminal auto-collapse:** the total terminal width is read from the
    statusbar's own `render` `cols` (zellij fires `render` on _every_ resize, but
    only sometimes a `PaneUpdate`). Below ~100 cols the panel folds; below ~76 the
    sidebar folds too (its threshold sits above zellij's ~64-col relayout floor,
    or its own presence would block the narrow relayout that triggers it). A pane
    is suppressed when either `manual` or `auto` holds; showing reapplies
    `next_swap_layout` to re-tile into the template slots.
