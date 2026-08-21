# Changelog

All notable changes to **thegn** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) with pre-release tags (`0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`). Release tags are `v<version>` on `main`.

## [Unreleased]

### Added — PR queue (team mode)

- **`thegn pr queue`** — the merge queue's counterpart for a repo other people
  push to. Queue a pull request, and thegn polls its state on the forge,
  classifies what is blocking it (red checks, a conflict with the base,
  requested changes, awaiting review), optionally hands that blocker to a
  configured agent in the PR's own worktree, and merges it once it is green.
  Panel section, statusbar chip, notifications, and `--json` on every verb.
  **Off by default** (`[pr_queue] enabled`): it is the one part of the shell
  that makes network _writes_.
- **The forge stays in charge of merging.** The default `merge_mode =
"auto_merge"` switches on the forge's own auto-merge, so branch protection,
  required reviews, and any server-side merge queue remain authoritative —
  thegn's view of "ready" can never race a rule it cannot see. `"thegn"` merges
  directly; `"ready"` never merges. A draft, or a PR without its required
  approval, is never merged either way.
- **Safe to point at a shared repo.** The agent pushes only with
  `--force-with-lease`; a push thegn did not make pauses it rather than racing a
  teammate (`pause_on_foreign_push`), and also refills its attempt budget so a
  long-lived PR never gets permanently stuck. A pull request you did not author
  is watched but never written to (`own_prs_only`). Review threads can be
  replied to, never resolved — that is the reviewer's call.

### Added — configurable agent handoff (both queues)

- **Prompts are now templates, not Rust.** `[merge_queue.prompts]` and
  `[pr_queue.prompts]` let you tell the fixing agent about your repo's
  conventions, per blocker kind. Leave a key empty for thegn's built-in
  instructions — which are byte-for-byte what previous versions sent, so
  nothing changes unless you opt in. Unknown placeholders are a `config
validate` error rather than a silent blank.
- **`agent = "claude"`** — name one of your `[[agents]]`/`[[tools]]` entries
  instead of restating its command with the right headless flags. thegn fills
  those in per provider (claude / codex / aider), and an agent it does not
  recognize still runs with the prompt appended. `agent_command` still wins
  when set, so any agent at all remains configurable.

### Fixed

- **`agent_command`'s documented example was wrong.** The shipped
  `config.toml.example` showed `claude -p "{prompt}"`, but placeholders are
  already shell-quoted during substitution — the agent received a prompt with
  literal quote characters wrapped around it. Placeholders must be written
  **bare**; `thegn config validate` now reports the mistake.

## [0.1.0-alpha.2] — 2026-08-20

A packaging and hygiene release. `v0.1.0-alpha.1` was tagged but never
published; this is the first release the public actually gets, and it fixes the
things a first-time cloner hit rather than anything about the running shell —
plus one genuine crash.

### Fixed — a process-wide abort in the metrics sampler

The system-metrics sampler could abort the whole of thegn — every pane in the
session — with:

```text
fatal runtime error: IO Safety violation: owned file descriptor already closed
```

It asked sysinfo to refresh a PID list that could name the same PID twice (when
the recorded pane-daemon PID equalled our own). sysinfo 0.39 fans that list
across a rayon pool by list position rather than by PID, so a repeated PID hands
two worker threads the same process entry at once; both take its cached
`/proc/<pid>/stat` handle and one closes a descriptor the other still owns. std
detects the double close and aborts.

Reaching it needed a daemon PID equal to ours — normally impossible, but
possible when a still-heartbeating registry row names a PID the OS has recycled
onto us. It was, however, unconditional in `thegn-metrics`' own unit test, where
it aborted ~5% of runs and reddened CI at random. The sampler now drops the
duplicate.

### Fixed — first-clone experience

- **`git clone --recurse-submodules` failed.** A stale `.gitmodules` still
  listed two private `apps/*` repos that are not reachable anonymously and are
  no longer submodules of this project. Removed.
- **Every `just` build printed a warning about private code.** The `_apps`
  recipe — a dependency of `build`, `quick`, `release`, `build-musl`, `lint`,
  `test`, `coverage` and `build-profiling` — warned that the host's chat/agent
  tabs would not build whenever no `apps/` checkout existed, which for everyone
  outside the maintainer's machine is always. Nothing depends on it.
- **The default agent picker offered a binary nobody has.** With no user
  config, `[[agents]]` was seeded with `termite`/`termite tui`, an unpublished
  TUI. The defaults are now `claude` and `shell`.
- **The project read as MIT-only.** A stray bare `LICENSE` (naming a different
  copyright holder than `LICENSE-MIT`) overrode GitHub's detection of the
  `MIT OR Apache-2.0` dual license declared in every manifest. The README now
  states the licensing too.

### Fixed — `install.sh`

- No longer deletes four pre-rename entry-point names and two `.desktop` files
  from your `bin` directory. No public user ever had the old brand, and one of
  those names is short enough to plausibly belong to an unrelated tool. An
  installer must not remove what it did not create.
- Installs a **copy** of the release binary instead of a symlink into
  `target/release/`. The symlink broke silently on `cargo clean`, a pruned
  worktree, or a moved repo. Re-run `./install.sh` after a rebuild.
- Fails loudly when the build produced no binary, instead of installing a
  dangling symlink.

### Fixed — CI and tooling

- **`pull_request` jobs no longer default to self-hosted runners.** On a public
  repo that fallback meant fork-PR code running on a personal machine against a
  persistent nix store, sccache and `CARGO_HOME`. GitHub-hosted is now the
  default and self-hosted is an explicit opt-in.
- **`NIX_GITHUB_TOKEN` is documented as optional.** Four places claimed it was
  required for "private flake inputs"; every flake input is public, and fork
  PRs — which receive no secrets — were being told their CI was unfixable.
- **Three gates were narrower than they claimed.** `doc-check` ran rustdoc
  without `--document-private-items` on a codebase that is overwhelmingly
  private, hiding seven broken intra-doc links and unclosed-HTML-tag errors
  (all fixed); `shellcheck` ran a hand-kept list that had drifted to 9 of 20
  tracked scripts, excluding the user-facing `setup-macos.sh`; and `taplo lint`
  was linting `.direnv`, `.devenv` and `target/` — 122 files, almost none ours.
- `CONTRIBUTING.md` now says that `just ci` must run under `nix develop`: the
  direnv/devenv shell's toolchain has neither `llvm-tools-preview` nor the
  cross targets, so `coverage` and `check-cross` fail there.

### Removed

Dead artifacts carrying no references: the Python LLM sidecar and its
agent adapters (`src/`, `tests/`), 196 directories of visual-regression
baselines for a harness that no longer exists (`snapshots/`), and ~195 KB of
mascot design-exploration scripts.

## [0.1.0-alpha.1] — 2026-08-18

The first public release: the AI-free workspace shell. Everything below is the
initial feature set rather than a delta.

### Platforms

Prebuilt binaries ship for **x86_64 Linux** (gnu + musl) only. Nix
(`nix profile install github:blakeashleyjr/thegn`) and `./install.sh` cover
Linux too.

macOS and Windows are **not supported** in this release and neither has been
run interactively. Windows compiles on msvc and passes its named-pipe IPC and
Job-Object tests — the repo was previously unclonable there, because a source
file used the reserved DOS device name `aux` — but its release build has not
yet completed inside the CI job budget. macOS has never been compiled at all:
it cannot be cross-checked from Linux, since `thegn-host`'s build scripts need
a real darwin C toolchain, and the macOS job cannot currently construct the dev
shell (the `openspec` derivation's `pnpm install` is OOM-killed on the runner).

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
  surface (`gh`-backed with native fallbacks). Worktree rows track the live
  HEAD branch, drag-and-drop reordering is scoped to folder-aware sibling runs,
  and click hit-testing resolves against the painted focus state.
- **Merge queue (“fold-actor”)** — `thegn merge` / `land` / `integrate` fold
  queued branches into the target branch in the object database (no target
  checkout), gate the folded tip on your test command in a reusable gate
  worktree, and advance the ref by compare-and-swap. Conflicts can be handed
  to any configured `agent_command` subprocess or deferred to you. Landing or
  dequeuing a worktree un-files it from its lifecycle folder.
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
  key, and `thegn config set` rolls back a value the loader would reject
  (without being blocked by a pre-existing bad value elsewhere in the file).
- **Config** — layered TOML (global → repo → env/profile overlays → env vars
  → `--set`), every key documented in `config/config.toml.example`, hot
  reload, and an in-app F1 help system with generated keybinding/config
  references. Per-zone key tables are single-sourced from the dispatch, so
  every live key surfaces in the help rather than drifting out of it.

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

[Unreleased]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.2...HEAD
[0.1.0-alpha.2]: https://github.com/blakeashleyjr/thegn/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/blakeashleyjr/thegn/releases/tag/v0.1.0-alpha.1
