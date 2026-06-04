# superzej

A terminal-native worktree IDE built on [zellij](https://zellij.dev). It recreates
the popular agentic-coding workflow — **one repo per tab, a fresh git worktree per
pane** — entirely in the terminal.

A single **Rust** binary (`superzej`, aliased **`sj`**) that orchestrates `git`,
`zellij`, and a fuzzy picker, with a bundled SQLite store for repo history and
worktree state. It's directory-agnostic — bare `sj` opens a launcher to pick a
**recent repo** (history) or add a new one (discovered under your `repoRoots` /
cloned from a URL), no matter where you are.

- **Workspace = tab.** `Alt-W` prompts for a repo (or clone a URL); the tab is
  named after it.
- **Worktree = pane.** `Alt-w` creates a new git worktree off the base branch and
  opens a pane in it, then shows a picker to choose a **coding agent**
  (claude/aider/…) or a plain shell.
- **Dashboard.** `Alt-d` opens a floating switcher listing every worktree with
  branch, age, ahead/behind and changed-file counts, with a live `git diff --stat`
  preview. `superzej dashboard --watch` is a pinnable, auto-refreshing pane.
- **Tools, scoped to the focused worktree, as floating panes:**
  `Alt-g` lazygit · `Alt-y` yazi · `Alt-e` `$EDITOR` · `Alt-/` git diff.
- **Cleanup.** `Alt-X` closes a pane and removes its worktree (branch kept by
  default). Plain pane-close never deletes a worktree.

## Keys

| Key   | Action                                 |
| ----- | -------------------------------------- |
| Alt-W | new workspace (open a repo as a tab)   |
| Alt-w | new worktree pane (+ agent picker)     |
| Alt-d | worktree dashboard (floating switcher) |
| Alt-g | lazygit (floating, scoped to worktree) |
| Alt-y | yazi                                   |
| Alt-e | `$EDITOR`                              |
| Alt-/ | git diff                               |
| Alt-X | close pane + remove worktree           |

## Install

### Nix / home-manager (recommended)

```nix
# flake.nix inputs
superzej.url = "github:youruser/superzej";

# home-manager config
imports = [ inputs.superzej.homeManagerModules.default ];
programs.superzej = {
  enable = true;
  worktreesDir = "/home/you/worktrees";
  repoRoots = [ "/home/you/code" "/home/you/src" ];  # scanned by the repo picker
  baseBranch = "auto";
  agents = [
    { name = "claude"; command = "claude"; }
    { name = "aider";  command = "aider --model sonnet"; }
    { name = "shell";  command = "__shell__"; }
  ];
};
```

This installs the `superzej` command and ships the layouts into
`~/.config/zellij/layouts/` — it **never touches your read-only `config.kdl`**.
Keybinds live inside the superzej layout and are merged for the session only.

### Standalone

```sh
./install.sh                 # cargo build --release + symlinks bin/sj + layouts
sj
```

Or, for Nix users, a fully-wrapped binary: `nix profile install .#default`.
superzej shells out to `git zellij fzf` (or `gum`); `lazygit yazi delta` are optional.

## How it works

- A single Rust binary; it **shells out** to `git`, `zellij action …`, and
  `fzf`/`gum`. State (repo history, tabs, worktrees) lives in a bundled-SQLite DB
  at `$XDG_STATE_HOME/superzej/superzej.db`.
- Keybinds are declared inside `layouts/superzej.kdl` (merged by zellij at session
  start), so the Nix-managed `config.kdl` stays untouched.
- There is no per-tab cwd in zellij, so the repo for a pane is **derived from its
  git context** (`git rev-parse --show-toplevel` / `--git-common-dir`) — robust
  across session resurrection; the DB is just a cache + history layer.
- Worktrees default to a global dir (`~/worktrees/<repo>/<branch-slug>`) to keep
  the repo tree clean; `worktreeMode = "in_repo"` uses `<repo>/.worktrees`.

## Config

See [`config/config.toml.example`](config/config.toml.example). Home-manager users
configure via `programs.superzej.*`; standalone users edit
`~/.config/superzej/config.toml`.

## Development

```sh
nix develop          # rust toolchain + tools
just build           # cargo build
just smoke           # hermetic end-to-end test
just ci              # fmt-check + clippy + build + test + smoke + nix-build
```

## Roadmap (Phase 2)

A small Rust→WASM `zellij-tile` sidebar plugin (`plugin/`) that renders the
worktree status live via `TabUpdate`/`PaneUpdate` events, fed by `zellij pipe` —
now able to share the `db`/`models` modules with the CLI.
