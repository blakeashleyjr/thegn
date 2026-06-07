# CLAUDE.md

Guidance for working in this repo. See `README.md` for the user-facing tour and
`tasks.md` for the roadmap / progress tracker.

## What this is

**superzej** (binary `superzej`, alias `sj`) — a terminal-native git-worktree IDE
built on [zellij](https://zellij.dev). One zellij **session** holds everything:
each repo is a `{slug}/home` **tab** (a _workspace_), each git **worktree** is a
`{slug}/{branch}` **tab**, ordinary panes are _panels_. A left WASM **sidebar**
switches tabs; a right WASM **panel** shows the focused worktree's diff + GitHub
PR state. Switching repos/worktrees is always a `switch_tab_to` — **never a
session change** (no teleport).

The long game (see `tasks.md`): superzej is two tracks joined by one keystone — an
**AI-free workspace shell** (the current, shippable product) and an **AI/agent
layer** bridged by an **LLM proxy**. The shell must never hard-depend on the AI
layers; AI is strictly additive (that's what makes "AI-free mode", items 511–515,
free).

## Architecture

- **Single Rust binary that shells out.** `superzej` orchestrates `git`,
  `zellij action …`, `gh`, and `fzf`/`gum`. Not a classic always-on daemon — the
  only long-running piece is the `watch` command (a `notify` fs-watcher that
  drives the panel's live diff refresh).
- **State.** Bundled SQLite at `$XDG_STATE_HOME/superzej/superzej.db` (repo
  history, workspaces, worktrees, a TTL'd PR cache). **git is the source of
  truth** for worktrees; the DB is a cache + history layer that survives session
  resurrection.
- **Managed zellij namespace.** superzej owns its config: seeds
  `~/.superzej/zellij.kdl` from `config/zellij.kdl` on first launch (**never
  overwritten** after) and starts zellij with `--config` it, isolated from the
  user's `~/.config/zellij`. Layouts under `~/.superzej/layouts/` _are_ re-seeded
  each launch. Worktrees default to `~/.superzej/worktrees/<repo>/<branch-slug>`.
- **`SUPERZEJ_SESSION`** marks "our world." `ZELLIJ_*` leaks into every child
  process, so superzej exports `SUPERZEJ_SESSION` before launching zellij; that —
  not the generic `ZELLIJ_*` vars — is how a `sj` invocation tells its own session
  from a foreign or leaked one. This prevents `sj` in any terminal from driving
  your real zellij session.
- **WASM plugins are sandboxed renderers** (`plugin/{sidebar,panel,tabbar,statusbar}`).
  Plugins can't shell out, so the panel drives the `superzej` binary via zellij's
  `run_command`/`pipe` bridge (`superzej pr status --json`, `superzej diff --stat`,
  `superzej resolve-worktree`). First-load permissions are pre-granted by
  `superzej grant-plugins` (run by the installers) — the permission prompt is
  un-approvable inside fixed/pinned panes.
- **Sandboxing.** Each worktree's interactive process runs in a container/sandbox
  by default (`podman` → `docker` → `bwrap` → `none`). The worktree stays on the
  host and is **bind-mounted into the sandbox at its real path** so host-side git
  reads (sidebar/panel/PR) keep working. Remote backend runs worktrees on another
  machine (mosh for the pane, ssh for git + container lifecycle).

## Source map

- `src/main.rs`, `src/cli.rs` — entry + clap command tree.
- `src/commands/*.rs` — one file per subcommand (the CLI surface the UI + plugins
  call). Notables: `new_worktree`, `new_workspace`, `pick_agent`, `pr`, `diff`,
  `watch`, `resolve`, `dashboard`, `monitor`, `stats`, `theme`, `grant_plugins`.
- `src/palette/` — the Cmd-K command palette: a native iocraft TUI (`menu`
  command), `nucleo` fuzzy matching + embedded ripgrep (`ignore`/`grep-*`).
- `src/{db,config,keymap,theme,sandbox,remote,github,worktree,repo,zellij}.rs` —
  the testable core (config layering, keymap/KDL, SQLite, sandbox backends, etc.).
- `src/log.rs` — hand-composed `tracing` subscriber (branded formatter + size-capped
  file sink).
- `plugin/*/src` — the four Rust→WASM `zellij-tile` plugins.
- `config/` — `config.toml.example` (every superzej key), `zellij.kdl` (seed), `yazi/`.
- `layouts/` — embedded zellij layouts (re-seeded each launch).
- `docs/superpowers/{plans,specs}/` — design docs per feature.

## Development

Run inside `nix develop` (provides the rust toolchain + `wasm32-wasip1` target + tools).

```sh
just build           # cargo build (binary only)
just build-plugins   # build the four WASM plugins
just test            # unit tests
just smoke           # hermetic end-to-end test
just e2e-ui          # plugin/chrome e2e (needs release + plugins)
just lint            # clippy + theme-sync check
just coverage        # cargo llvm-cov, gated at 95% lines on the core
just ci              # fmt-check + lint + build + plugins + test + coverage + smoke + nix-build
```

Nix: `nix profile install .#default` (wrapped binary); plugins via
`nix build .#superzej-{sidebar,panel,tabbar}`. `nix develop` for the dev shell.

## Conventions & gotchas

- **Testable core is gated at 95% line coverage** (`src/{config,keymap,db,...}`);
  I/O / subprocess / WASM glue is excluded from coverage and exercised by
  `test/smoke.sh` + the e2e suite instead. See `docs/coverage.md`. New core logic
  needs unit tests.
- **The theme palette is a _copied_ `theme.rs`**, not a shared crate — Nix
  sandboxes the plugin subdirs, so the palette is duplicated into each plugin and
  kept in sync via `just sync-theme` (checked by `just check-theme`/`lint`). Only
  the `accent` is configurable; the rest of the storm-blue palette is fixed.
- **This shell often runs _inside_ a live superzej.** zellij-spawning e2e tests
  leak into the daily DB / shared socket unless sandboxed — isolate
  `ZELLIJ_SOCKET_DIR` (not just `XDG_STATE_HOME`), and `pkill -9` the sandbox
  zellij server on cleanup (delete-session leaves zombies; a runaway server can
  pin CPU >1000%). The e2e harness (`test/nav-ux.py`) is fully self-contained.
- **`.pre-commit-config.yaml` is a gitignored Nix store symlink** (managed by
  devenv) — don't edit it directly. `git add` new files before `nix flake check`.
- Commit/push only when asked; branch off `main` first (see `/branch`). Conventional
  commit style (`feat(scope):`, `fix(scope):`) matches the history.
