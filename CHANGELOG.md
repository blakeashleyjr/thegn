# Changelog

All notable changes to **thegn** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) with pre-release tags (`0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`). Release tags are `v<version>` on `main`.

## [Unreleased]

## [0.1.0-alpha.1] — 2026-08-15

The first public release: the AI-free workspace shell. Everything below is the
initial feature set rather than a delta.

### The shell

- **Worktree-native multiplexing** — each repo is a workspace, each git
  worktree is a tab; panes, splits, zoom, copy mode, scrollback search, and a
  command palette, all rendered in-process (no tmux/zellij underneath).
- **Pane daemon** — PTYs are owned by a background `thegn daemon` process
  (unix socket), so quitting the UI detaches sessions and the next launch
  warm-reattaches them, scrollback intact. `thegn serve` exposes the same
  sessions to remote thin clients over TCP (token-auth). Disable with
  `[daemon] enabled = false` for fully in-process panes.
- **Git, first-class** — sidebar tree with live status/activity, diff and PR
  panels, blame, semantic (entity-level) diff summaries, visual staging,
  branch actions, undo/redo of repo operations, and a GitHub PR/issue/CI
  surface (`gh`-backed with native fallbacks).
- **Merge queue (“fold-actor”)** — `thegn merge` / `land` / `integrate` fold
  queued branches into the target branch in the object database (no target
  checkout), gate the folded tip on your test command in a reusable gate
  worktree, and advance the ref by compare-and-swap. Conflicts can be handed
  to any configured `agent_command` subprocess or deferred to you.
- **Sandboxing** — each worktree's interactive process can run in a container
  (`podman` → `docker` → `bwrap` → `none` auto-chain) with hardening presets
  (`open`/`hardened`/`sealed`/`sealed-tunnel`), egress allow/block lists, a
  VPN sidecar option, and devshell/direnv injection. The worktree stays on
  the host so git reads keep working.
- **Sessions that survive** — worktrees, tabs, layouts, terminal order, pins,
  and sidebar state persist in SQLite and resurrect on launch; git remains
  the source of truth.
- **Launcher picker** — creating a worktree opens a tab and prompts for what
  to run there: a shell, or any configured `[[agents]]`/`[[tools]]` entry
  (claude, aider, lazygit, yazi, …) launched as an ordinary command.
- **Terminal compatibility** — capability detection with graceful
  degradation: truecolor→256→16→mono, Unicode→ASCII glyphs, undercurl and
  mouse feature-gating. `thegn doctor` prints what was detected.
- **Performance invariants** — sub-300ms launch to first frame, damage-region
  rendering (<16ms frames), and ~0% idle CPU (the idle loop blocks; it never
  polls). Enforced by unit-tested render-plan invariants in CI. Pane spawns
  (including crash respawns and the new-terminal wizard) resolve their sandbox
  off-thread, and pane stdin rides a bounded per-pane writer queue — a
  flow-stopped or wedged child can never freeze the UI (a dropped paste
  surfaces as a toast instead).
- **Input safety** — bracketed pastes are delivered to the child as one atomic
  chunk with embedded `ESC[200~`/`ESC[201~` markers neutralized on every paste
  path, so pasted content can't close the bracket early and inject keystrokes.
- **CLI** — a stable, documented exit-code contract; worktree targeting
  unified on a canonical `--worktree` flag across verbs (legacy positionals
  still parse); `thegn config validate` strict-checks every enum-valued config
  key, and `thegn config set` rolls back values the loader would reject.
- **Config** — layered TOML (global → repo → env/profile overlays → env vars
  → `--set`), every key documented in `config/config.toml.example`, hot
  reload, and an in-app F1 help system with generated keybinding/config
  references.

### Channels

The stable build ships the shell above. Experimental subsystems (remote
worktrees over SSH, cloud execution providers, Observe dashboards, the
placement engine, non-GitHub trackers) are compiled in but disabled outside
the dev channel (`THEGN_CHANNEL=dev` / the `thegn-dev` build).

### Removed before this release

The in-repo AI/agent layer (an LLM proxy daemon, an ACP-embedded agent with a
tool-approval “bouncer”, and a managed agent install) was removed from the
codebase prior to this release to ship a focused, AI-free shell. The generic
launcher picker and the merge queue's `agent_command` hook remain — they run
whatever commands you configure. Leftover `[llm_proxy]` config sections and
`proxy_*` tables in existing state databases are ignored harmlessly.

[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.1]: https://github.com/blakeashleyjr/thegn/releases/tag/v0.1.0-alpha.1
