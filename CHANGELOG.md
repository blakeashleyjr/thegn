# Changelog

All notable changes to **thegn** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) with pre-release tags (`0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`). Release tags are `v<version>` on `main`.

## [Unreleased]

### Added — a stopped runtime is a question, not a silent downgrade

- **`[sandbox] on_dormant`.** A container runtime that is installed but not
  running — a stopped `dockerd`, a `podman machine` nobody started, colima down —
  was folded into "absent" by the backend chain, and the pane opened on the host
  without a word. thegn already knew the difference (`BackendState::NotRunning`
  carries a remedy) but only said so in the onboarding wizard, which is the one
  moment you are not launching anything. Now a launch that asked for containment
  and would degrade offers the fix: **[s] start it · [h] run on host · [n]
  cancel**, with the start command shown verbatim before you approve it.
- Policy is configurable: `ask` (default), `start` (run it unattended and
  re-resolve), `host` (the old silent degrade, now truthfully labelled), or
  `cancel` (refuse to run uncontained). `start` falls back to `ask` for a runtime
  with no unattended start — rootful podman needs a password, and a launch path
  will not prompt for one.
- An `auto`/`host` launch that lands on the host is the configured outcome, not a
  degradation, and never raises the prompt.
- Starting runs off the event loop with a bounded 90s budget (booting a VM is
  slow), then drops the probe cache so the retry re-probes instead of replaying
  the cached "absent" that caused the degrade.
- **The macOS Docker remedy was wrong**: it told Mac users to run `systemctl`,
  which does not exist there, and never mentioned colima. The remedy sentence and
  the start command are both OS-aware now.

### Fixed — the macOS release process, rehearsed end to end

Built a real macOS release locally and ran it through the documented path. Three
things the docs claimed turned out to be wrong or imprecise:

- **`brew install --formula ./packaging/homebrew/thegn.rb` does not work.** Modern
  Homebrew rejects a formula given as a file path ("Homebrew requires formulae to
  be in a tap"), and both `RELEASING.md` and `KNOWN_ISSUES.md` told users to do
  exactly that. Replaced with the `brew tap-new` recipe, which was then used to
  install the real artifact end to end — formula installs, caveats render, `thegn`
  and `tg` both run from the brew prefix, and the installed binary carries **no**
  `com.apple.quarantine`, confirming the claim the whole no-notarization decision
  rests on.
- **"Unsigned" was imprecise.** Apple silicon requires a signature to execute, so
  the linker ad-hoc signs every arm64 binary (`codesign -dv` → `adhoc,
linker-signed`). That is not a Developer ID signature and does not satisfy
  notarization — `spctl -a` rejects it either way — but the docs now say what is
  actually true.
- **A quarantined binary hangs rather than failing.** It stalls on a Gatekeeper
  dialog. And once macOS has denied it, the verdict is cached: removing the
  attribute afterwards is not always enough. So the docs now say to clear it
  _before_ the first run, and name the recovery (System Settings → Privacy &
  Security, or a fresh path).

- **Release archives shipped no license text.** thegn is `MIT OR Apache-2.0` and
  the archive contained only the binary — the upload action includes nothing else
  by default. Both licenses and the README now ride along; the binary stays at the
  archive root, which is what the formula's `bin.install` requires.
- **`just release-artifacts <tag>` / `just release-verify <tag>`** reproduce the
  CI archive byte-shape locally (root layout, `shasum -a 256` fallback, no
  `.tar.gz` infix on the checksum file) and assert it. With remote CI paused this
  is the only way to find out a release build is broken before the tag is public,
  so `RELEASING.md` now has it as a step.

### Added — macOS reaches users, not just this machine

- **The macOS CI job is re-enabled.** It was disabled outright because its first
  real run OOM-killed building `openspec` — a tool the job never invokes. A new
  lean `devShells.ci` (toolchain, just, nextest, pkg-config, zlib) carries only
  what `just build` and `just test` use, removing the failure and cutting the
  cost. It stays off by default on the same `extras` gate as the `windows` and
  `e2e` jobs — macOS runners bill at 10x, and while dispatch is the only way CI
  runs at all, a bare dispatch must not quietly cost ten times more. Run it with
  `gh workflow run ci.yml --ref <branch> -f extras=true`.
- **`aarch64-apple-darwin` rejoins the release matrix**, so the next tag ships
  Apple-silicon binaries alongside linux-gnu and linux-musl.
- **The Homebrew formula is release-ready**: Apple-silicon only (matching the
  matrix), with caveats that tell the user how to generate the `thegn.app`
  launcher and how to make Option send Alt. `RELEASING.md` documents the exact
  tap layout so step 6 is mechanical.
- **A decision, written down: thegn does not sign or notarize.** Notarization
  costs a paid Apple Developer account and a signing key in CI. The supported
  macOS paths are chosen so it is not needed — Homebrew formula downloads, Nix
  store paths, and the locally generated `thegn.app` are none of them
  quarantined. Only a tarball fetched through a browser is, and both
  `RELEASING.md` and the README now say so plainly, with the `xattr` escape and
  the conditions under which the decision should be revisited.

### Fixed — thegn's own chords were untypeable on macOS

- **The bundled terminal profiles now map Option to Alt.** thegn's primary
  chords are Alt-based (`Alt-w` new worktree, `Alt-o` switch, `Alt-s` sidebar,
  `Alt-.` panel, plus every `Ctrl-Alt` chrome toggle), and macOS composes
  characters with Option by default — so `Alt-w` typed `∑`, nothing happened,
  and the key read as dead rather than unbound. `config/alacritty.toml`
  (`option_as_alt = "Both"`, where Alacritty's default of `None` meant thegn
  shipped a profile that could not type thegn's own keymap) and
  `config/ghostty.config` (`macos-option-as-alt = true`) both set it now, so
  `tg --standalone` and the generated `thegn.app` work out of the box.
- For the terminal **you** launch thegn in, the setting is yours to make: the
  in-app help (Terminal compatibility) and `KNOWN_ISSUES.md` list it for
  Ghostty, Alacritty, kitty, WezTerm and iTerm2.

### Fixed — a pane could claim a sandbox it did not have

- **Containment is now derived from the argv a pane executes, never from the request.** A terminal
  created with an explicit `podman-rootless` pick, on a host with no podman machine running,
  resolved through the chain to `Backend::None`, spawned a bare `sh -lc 'cd … && exec $SHELL'` —
  and was still labelled `podman-rootless`, because the label was copied from the pick. A label
  that can disagree with reality is worse than no label: it says "sandboxed" about a pane running
  on the host with no kernel boundary. The new `thegn_core::sandbox_truth` module reads the backend
  out of the command that actually runs and reconciles it against what was asked for, producing the
  label, a degraded flag, and a warning; `panes.rs` and `agent.rs::compose_spec` both go through it,
  and a degraded terminal now falls through to a plainly labelled host shell.
- **The gate that keeps it fixed**: `every_backend_round_trips` renders the real `enter_argv` for
  every `Backend` and asserts the derived label matches, over a list that is exhaustive by
  construction — adding a backend without extending the gate fails to compile. Companion tests pin
  the dangerous direction: a worktree path, git remote, or image reference named `docker` must
  never promote a host shell into a claimed container.

- **Recorded intent and observed containment are now separate columns.** The wizard's pick was
  written to `terminals.sandbox_backend` / `worktrees.sandbox_backend` before anything launched,
  and every surface displayed that column — so the chip reported the pick as fact. The pick stays
  where it is (it is a deliberate override that drives re-resolution, so a user who later starts
  their runtime still gets the sandbox they asked for), and a new `observed_backend` column records
  what each launch actually entered. The tab chip, the sidebar rows for both worktrees and
  terminals, and `active_backend` all read the observed value.
- The tab chip no longer **predicts**. It used to show the backend config would resolve to before a
  worktree had ever launched, so it was never empty; that was a claim rendered as fact. A chip that
  is briefly empty is honest, and it fills in the moment a pane launches.

### Added — macOS app launcher

- **`thegn.app`, generated locally** (`packaging/macos/make-app.sh`). macOS has
  no freedesktop registry, so `install.sh` used to opt darwin out of launcher
  integration entirely and leave Mac users with nothing to search for. It now
  detects the platform and writes a `thegn.app` bundle into `~/Applications`
  instead — indexed by Spotlight, Raycast, Alfred and the Dock — which opens the
  first terminal it finds (Ghostty → WezTerm → kitty → Alacritty → Terminal.app)
  running thegn through a login shell, so the tools thegn shells out to are on
  `PATH` under launchd's bare environment. `just macos-app` generates the same
  bundle for the Nix and Homebrew installs, which never run `install.sh`.
  Generating on the machine rather than shipping a prebuilt bundle is what keeps
  Gatekeeper out of the way: no `com.apple.quarantine`, so no Developer ID
  signing or notarization is needed to open it.
- **`--env KEY=VALUE`** bakes environment into a bundle, so a second,
  side-by-side launcher can run a debug binary with `THEGN_LOG` / `THEGN_PERF`
  and an isolated `XDG_STATE_HOME`.
- **`packaging/macos/thegn.icns`** — the owl app icon for the bundle, rendered
  from the same `owl.rs` sprite as `config/thegn.svg` by
  `scripts/gen-owl-icns.py` (pure stdlib: no rasterizer, no `iconutil`, so it
  also runs on Linux). `just icons` regenerates both.
- The `install.sh` summary no longer claims to have written a `.desktop` entry
  and an hicolor icon on platforms where it wrote neither.

### Added — macOS development (Apple silicon + nix-darwin)

- **`darwinModules.default`** — a nix-darwin module that puts thegn on PATH
  system-wide (`programs.thegn.enable`). It is deliberately thin: nix-darwin has
  no per-user config-file mechanism, so configuration stays with
  `homeManagerModules.default`, which already worked on darwin. Enabling both is
  the intended shape and they install the same store path. See the README's
  "nix-darwin (macOS)" section.
- **The flake's darwin outputs evaluate.** `packages.sandbox-image`,
  `fly-sandbox-image` and `thegn-musl` are now gated to Linux — the two OCI
  images could not even be _evaluated_ on darwin (`shadow`/`procps` refuse a
  darwin hostPlatform), which took `nix flake show` and `nix flake check` down
  with them. The systems list also drops `x86_64-darwin`, which the pinned
  nixpkgs has retired and which therefore threw on every Intel-mac attribute.
- **`devenv shell` no longer builds a cross-compiler on a Mac.** The mingw-w64
  cross-CC env vars were set unconditionally, so entering the shell on darwin
  forced a from-source mingw GCC build — and `.envrc` prefers devenv, so that
  was the shell a Mac contributor landed in. Now `isLinux`-gated, matching the
  flake's `mingwCrossEnv`.
- **The `openspec` derivation caps its memory** (`NODE_OPTIONS`, pnpm
  child-concurrency). Its `pnpm install` being OOM-killed on the 7 GB macOS
  runner is why the `[ci-macos]` job had never reached thegn at all.

### Fixed — macOS runtime parity

- **Activity dots work on macOS.** The per-worktree CPU scanner had a Linux
  `/proc` arm, a Windows `sysinfo` arm, and an empty stub for everything else —
  so dots never lit on a Mac. The `sysinfo` arm now serves every non-Linux
  platform.
- **`open` instead of a hardcoded `xdg-open`.** "Open in browser" now honours
  `$BROWSER` and falls back to the platform opener, which is what the
  `[forward] browser` docs had always promised and the code never did.
- **`apple` joins the default sandbox chain**, after `docker` and before
  `bwrap`. A Mac with Apple's `container` installed silently resolved `auto` to
  an unsandboxed host pane; the backend was fully wired but unreachable without
  naming it explicitly. Its probe is macOS-gated, so no other platform changes.
- **Pane cwd and foreground-command capture work on macOS**, via a new
  `platform::proc` seam (libproc/`sysctl`, no new dependency) behind what were
  bare `/proc` reads. Restores "respawn panes where they were" and "relaunch
  what was running" off Linux.
- **The font picker degrades** to scanning the standard macOS font directories
  when fontconfig (`fc-list`) is absent, instead of dead-ending.
- A watcher failure no longer blames "inotify watches exhausted" on platforms
  where `notify` rides FSEvents rather than inotify.

### Changed — dev loop portability

- **`just check-cross` covers six crates on darwin, not two** — every crate that
  builds without a darwin cross C toolchain. Each leg now skips loudly when its
  toolchain is missing instead of failing, so `just ci` is runnable on a Mac
  (where the mingw cross-cc is deliberately absent) and in `devenv shell`.
- **GNU-userland assumptions removed** from the dev loop: `sed -i`, `setsid`,
  `script -qec` (util-linux vs BSD, now via `test/lib/pty.sh`), `sha256sum`,
  and an `aarch64`-only `uname -m` case that Apple silicon reports as `arm64`.
  `install.sh` skips the freedesktop `.desktop`/icon files on darwin, and
  `test/perf/cpu-sample.sh` (hard `/proc` dependency) skips with a clear message.

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
