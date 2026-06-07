# superzej

A terminal-native worktree IDE built on [zellij](https://zellij.dev). Each git repo
is its own zellij **session** (a _workspace_), each git **worktree** is a **tab**, and
ordinary zellij **panes** are your _panels_. A left **sidebar** lists and switches
between workspaces; a right **panel** shows the focused worktree's git diff and full
**GitHub PR** state (checks, review, merge) — entirely in the terminal.

A single **Rust** binary (`superzej`, aliased **`sj`**) orchestrates `git`, `zellij`,
`gh`, and a fuzzy picker, with a bundled SQLite store for repo history and worktree
state, plus three Rust→WASM `zellij-tile` plugins (the sidebar, the diff/PR panel,
and a custom branch-tab strip). It's
directory-agnostic — bare `sj` opens a launcher to pick a **recent repo** (history) or
add a new one (discovered under your `repoRoots` / cloned from a URL).

## Mental model

superzej is **one zellij session**; everything is a tab in it, so switching between
repos/worktrees is a tab switch — never a session change (no teleport).

| Concept       | Maps to                        | Created by                                                                |
| ------------- | ------------------------------ | ------------------------------------------------------------------------- |
| **Workspace** | a repo's `{slug}/home` **tab** | `Alt-W` — pick/clone a repo (opens its home tab)                          |
| **Worktree**  | a `{slug}/{branch}` **tab**    | `Alt-w` — new worktree off the base branch, then a picker for what to run |
| **Panel**     | a plain zellij **pane**        | `Alt-n` split — behaves like any zellij pane                              |
| **Sidebar**   | left WASM plugin               | repo → worktree → tabs tree; click to switch (`Alt-s` hides)              |
| **Diff / PR** | right WASM plugin              | tracks the focused tab's worktree (`Alt-p` hides)                         |

- **Worktree = tab.** `Alt-w` creates a new git worktree off the base branch and opens
  a tab named after the branch, then prompts for a **coding agent** (claude/aider/…),
  a tool, or a plain shell.
- **Right panel.** For the focused worktree: `git diff --stat`, and the branch's PR —
  number/title/state, CI check rollup, review decision — with action keys:
  `o` open · `c` create · `m` merge · `a` approve · `r` re-run failed checks · `f` refresh.
- **Tools, scoped to the focused worktree, as floating panes:**
  `Alt-g` lazygit · `Alt-y` yazi · `Alt-e` `$EDITOR` · `Alt-/` git diff.
- **Cleanup.** `Alt-X` removes the focused worktree and closes its tab (branch kept by
  default). Closing a plain panel never deletes a worktree.

## Keys

Context-relevant keys are always shown in the bottom **status bar**, and
**Cmd-K** (Super-K) opens a fuzzy **command palette** of every action below.
The center column is otherwise vanilla zellij — split / stack / move / resize
panes, pane & tab modes. `Alt-[` / `Alt-]` cycle the center terminals through
vertical / side-by-side / stacked arrangements (needs ≥2 center panes; the
sidebar, tabbar and panel stay pinned).

| Key               | Action                                                   |
| ----------------- | -------------------------------------------------------- |
| Cmd-K (Super-K)   | command palette (fuzzy menu of all actions)              |
| Alt-←/→           | switch tabs                                              |
| Alt-h/j/k/l       | move pane focus (sidebar ↔ terminals ↔ panel)            |
| Super-Alt-←/→/h/l | same, across columns (needs a WM that forwards Super)    |
| Super-Alt-↑/↓/j/k | same, between stacked terminal panes                     |
| Alt-W             | new workspace (open a repo as its home tab)              |
| Alt-w             | new worktree (a tab + "what to run" picker)              |
| Alt-t             | new tab on the _same_ worktree (`{tab} ·2`, full chrome) |
| Alt-n             | new panel (plain split pane)                             |
| Alt-o             | switch workspace (floating repo picker)                  |
| Alt-d             | worktree dashboard (jump to any worktree tab)            |
| Alt-s             | hide / show the left sidebar                             |
| Alt-p             | hide / show the right diff/PR panel                      |
| Alt-g             | lazygit (floating, scoped to worktree)                   |
| Alt-y             | yazi                                                     |
| Alt-e             | `$EDITOR`                                                |
| Alt-/             | git diff                                                 |
| Alt-X             | remove worktree + close tab                              |

_The above uses the `Normal` keybind mode. Superzej also ships with native `VimNormal` (with `Space` leader layer) and `Emacs` mode presets; switch modes with `Ctrl-Alt-v` or `Ctrl-Alt-e`._

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

This installs the `superzej` command and deploys the three WASM plugins to
`~/.local/share/superzej/{sidebar,panel,tabbar}.wasm`. The layouts and the
zellij config are embedded in the binary and seeded into superzej's own
namespace — `~/.superzej/layouts/` and `~/.superzej/zellij.kdl` — at launch, so
superzej never touches your personal `~/.config/zellij`. superzej starts zellij
with `--config ~/.superzej/zellij.kdl` for full control — edit that file to
customize; it is **never overwritten** once it exists. (The layouts _are_
re-seeded each launch, so they track the installed build.)

### Standalone

```sh
./install.sh    # builds the binary + both WASM plugins, symlinks bin/sj, layouts, plugins
sj
```

`./install.sh` needs the `wasm32-wasip1` rust target (it runs `rustup target add` for
you). For Nix users, a fully-wrapped binary: `nix profile install .#default`, and
plugins via `nix build .#superzej-sidebar .#superzej-panel .#superzej-tabbar`. superzej shells out to
`git zellij fzf gh` (or `gum`); `lazygit yazi delta` are optional.

## How it works

- A single Rust binary that **shells out** to `git`, `zellij action …`, `gh`, and
  `fzf`/`gum`. State (repo history, workspaces, worktrees, a TTL'd PR cache) lives in a
  bundled-SQLite DB at `$XDG_STATE_HOME/superzej/superzej.db`; the managed zellij config
  and worktrees live under `~/.superzej/`.

- **Managed config & full control.** superzej owns the zellij config: it seeds
  `~/.superzej/zellij.kdl` from a default on first launch (never overwriting your edits)
  and starts zellij with `--config` it. So keybinds, options, and theme are yours to
  customize in one file, independent of your global `~/.config/zellij/config.kdl`.

- **One session; repos are tabs.** The whole interface is a single zellij session.
  Each repo is a **tab** named `{slug}/home` (its main checkout); worktrees are
  `{slug}/{branch}` tabs. _Inside_ the session, opening or selecting a repo is a
  `switch_tab_to` — the sidebar/tabbar/panel stay put and only the middle terminal +
  right panel change. **No session switch, ever** (that was the teleport). Tab names
  are repo-scoped so they stay globally unique, which is also how the panel /
  `resolve-worktree` key a tab back to its worktree. From _outside_ a superzej session,
  launching **cold-starts the one session** (rooted at the first repo) — first stripping
  any inherited `ZELLIJ_*` env so it never nests into or hijacks a foreign session. The
  session layout pins the sidebar/tabbar/panel plugins around each tab's terminal.

- **`SUPERZEJ_SESSION` — knowing "our world".** `ZELLIJ_SESSION_NAME` leaks into every
  process spawned from a pane (a new terminal inherits it), and nothing in the env or
  process tree distinguishes a real pane from a leaked child. So superzej exports
  `SUPERZEJ_SESSION` before launching zellij; every pane of a superzej session inherits
  it, and **that** — not the generic `ZELLIJ_*` vars — is how superzej tells its own
  sessions from a foreign (or leaked) one. This is what prevents a `sj` in any terminal
  from accidentally driving your main zellij session.

- **The WASM plugins are sandboxed renderers:** the **sidebar** lists every managed repo
  (live tabs from `TabUpdate`, plus closed repos pulled via `superzej workspaces`) and
  switches **tabs**; the **panel** drives the `superzej` binary via zellij's
  `run_command`/`pipe` bridge (`superzej pr status --json`, `superzej diff --stat`,
  `superzej resolve-worktree`) because plugins can't shell out themselves. First-load
  permissions are pre-granted by `superzej grant-plugins` (run by the installers).

- git is the source of truth for worktrees; the DB is a cache + history layer, robust
  across session resurrection. Worktrees default to `~/.superzej/worktrees/<repo>/<branch-slug>`;
  `worktreeMode = "in_repo"` uses `<repo>/.worktrees`.

## Config

- **superzej behavior** — see [`config/config.toml.example`](config/config.toml.example).
  Home-manager users configure via `programs.superzej.*`; standalone users edit
  `~/.config/superzej/config.toml`.
- **accent color** — `[theme] accent = "#76eede"` in `config.toml` (or
  `programs.superzej.themeAccent`) recolors every superzej surface: the plugin
  chrome, pickers, dashboard, and `list`. The rest of the storm-blue palette is
  fixed.
- **zellij behavior** (keybinds, options, theme) — edit the managed config at
  `~/.superzej/zellij.kdl` (seeded from [`config/zellij.kdl`](config/zellij.kdl); never
  overwritten once it exists). To adopt a new shipped theme on an existing
  install, delete that copy so it re-seeds (and `zellij delete-session superzej`
  if a stale serialized session lingers).

## Sandboxing worktrees

By default each worktree's interactive process (agent / shell / tools) runs inside
a container or sandbox, so a coding agent can't reach your whole machine. The git
worktree itself stays on the host and is **bind-mounted into the sandbox at its
real path** — so the sidebar/panel/PR (which read git host-side) keep working,
while the agent only sees the worktree and its git metadata.

- **Backends** (`[sandbox] backend`): `auto` walks `backend_chain` and picks the
  first available — `podman` (rootless, preferred) → `docker` → `bwrap`
  (lightweight, reuses host tools, no image) → `none` (plain host, with a
  warning). `systemd`, `apple` (macOS `container`), and `wsl` are also selectable.
  Setting an `image` switches `auto` to the OCI runtimes; leaving it empty uses
  the host-toolchain sandbox (bwrap/systemd).
- **Per-repo** — drop a `.superzej.{toml,yaml,yml,json}` at a repo root with a
  `[sandbox]` table to override the global config (image, init script, mounts,
  `devenv`, remote, …). A repo with `devenv.nix` auto-wraps the shell in
  `devenv shell` when `devenv` is installed.
- **Remote** (`[sandbox.remote] host`) — run worktrees on another machine. The
  interactive pane goes over **mosh** (low-latency, roaming); git reads and
  container lifecycle go over **ssh**. Composes with the container backends (mosh/
  ssh → podman on the remote).
- **Opt out** — `backend = "none"` (or `enabled = false`) runs on the host as
  before. Auth/egress: agents need network and credentials, so `network` defaults
  to `nat` and `env_passthrough` forwards `SSH_AUTH_SOCK`/tokens.

See [`config/config.toml.example`](config/config.toml.example) for every key.

## Development

```sh
nix develop          # rust toolchain (+ wasm32-wasip1) + tools
just build           # cargo build (the binary)
just build-plugins   # cargo build both WASM plugins
just smoke           # hermetic end-to-end test
just ci              # fmt-check + clippy + build + plugins + test + smoke + nix-build
```
