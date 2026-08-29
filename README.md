# thegn

> **Status: public alpha** (`0.1.0-alpha.2`). **x86_64 Linux is the supported
> platform.** macOS on Apple silicon now builds, tests and runs, has a generated
> `thegn.app` launcher, and rejoins the release matrix from the next tag — treat
> it as best-effort, not supported: its CI job is opt-in and the interactive
> on-device checklist is unfinished. Windows compiles and passes its tests on
> msvc but has never been run interactively, and ships no binaries. Expect rough
> edges; see [`CHANGELOG.md`](CHANGELOG.md) and
> [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md), and please file issues.

A terminal-native git-worktree IDE that is **its own terminal multiplexer**.
One session: each git repo is a **workspace**, each git **worktree** is a
**tab**, and the chrome — a left sidebar tree, a right diff/PR panel, masthead,
statusbar, and a pinned-program strip — is rendered in-process by a native
compositor. No plugins, no external multiplexer. (thegn was originally built
on zellij; that architecture was fully stripped and it is now a from-scratch
native compositor.)

A single **Rust** binary (`thegn`, with a short `tg` alias) drives
portable-pty panes, composites them with the chrome into a termwiz surface,
and diff-flushes to your terminal. Panes are owned by a background **pane
daemon**, so quitting the UI detaches your sessions and the next launch
reattaches them, scrollback intact (tmux semantics — disable with `[daemon]
enabled = false`). A bundled SQLite store keeps repo history, worktree state,
session layout, and a PR cache. Everything is instant by construction:
sub-300ms launch, <16ms renders, ~0% idle CPU.

## Mental model

thegn is **one session**; switching repos or worktrees is a tab switch,
never a session change.

| Concept       | Maps to                       | Created / toggled by                                                    |
| ------------- | ----------------------------- | ----------------------------------------------------------------------- |
| **Workspace** | a repo                        | `Alt-W` — pick/clone a repo                                             |
| **Worktree**  | a git worktree = a tab        | `Alt-w` — new worktree off the base branch, then a "what to run" picker |
| **Pane**      | a terminal split within a tab | `Alt-p` smart split (`Alt-n` down, `Alt-N` right)                       |
| **Sidebar**   | native left tree              | repo → worktree → tabs; `Alt-s` focus, `Ctrl-Alt-s` hide                |
| **Diff / PR** | native right panel            | tracks the focused worktree; `Alt-.` focus, `Ctrl-Alt-p` hide           |
| **Pins**      | daemon panes in a top strip   | `Ctrl-Alt-1..9` launch/focus a `[[pins]]` program                       |

- **Worktree = tab.** `Alt-w` creates a new git worktree off the base branch,
  opens a tab named after the branch, and prompts for what to run there — a
  plain shell, or any configured `[[agents]]` / `[[tools]]` entry (claude,
  aider, …) launched as an ordinary command — optionally inside a sandbox
  (see below).
- **Right panel.** For the focused worktree: the git diff and the branch's PR —
  state, CI check rollup, review decision — plus CI runs, the merge and PR
  queues, notifications, and shares, organized in panel tabs.
- **Tools, scoped to the focused worktree:**
  `Alt-g` lazygit · `Alt-e` `$EDITOR` · `Alt-/` git diff ·
  `Ctrl-Alt-f` / `Alt-y` bottom files drawer.
- **Quick-jump digits:** `Alt-1..9` jump to a worktree, `Ctrl-1..9` to a
  workspace, by their slot in sidebar order. `Ctrl-Alt-1..9` launch or focus
  pinned programs.
- **Cleanup.** `Alt-x` is one smart **close** — the focused pane if the tab is
  split, otherwise the tab. `Alt-X` (Shift) escalates to removing the whole
  worktree and its tab (branch kept). Close never deletes a worktree unless you
  reach for the Shift variant.

## Keys

Context-relevant keys are shown in the bottom **status bar**, and
**Ctrl-Space** opens a fuzzy **command palette** of every action — the palette
is the complete, always-current reference. A leading `~` switches it to the
**frecency opener**: workspaces + worktrees ranked by how often _and_ how
recently you use them — type a fragment, Enter lands in that worktree's tab.
The palette also carries **Connect to root** (jump from a shell nested deep in
a subdir straight to the owning worktree's tab) and **Clone and open** (paste
a git URL; it clones off-loop and opens as a workspace). Existing
tmuxinator/sesh project files show up automatically as new-worktree templates.
Defaults (override via `[keybinds]`):

| Key                     | Action                                                   |
| ----------------------- | -------------------------------------------------------- |
| Ctrl-Space              | command palette (fuzzy menu of all actions)              |
| Alt-←/→                 | previous / next tab (within the worktree)                |
| Alt-↑/↓                 | previous / next worktree (within the workspace)          |
| Shift-Alt-↑/↓           | previous / next workspace                                |
| Ctrl-←/↓/↑/→ (h/j/k/l)  | move focus: sidebar ↔ panes ↔ panel (↑/↓ reach the bars) |
| Alt-\`                  | bounce between workspaces region and terminals region    |
| Alt-w / Alt-W           | new worktree ("what to run" picker) / new workspace      |
| Alt-t / Alt-T           | new tab on the _same_ worktree / new terminal tab        |
| Alt-p / Alt-n / Alt-N   | new pane: smart split / split down / split right         |
| Alt-o                   | switch workspace                                         |
| Alt-s / Alt-.           | focus sidebar / focus panel                              |
| Ctrl-Alt-s / Ctrl-Alt-p | hide/show sidebar / diff-PR panel                        |
| Ctrl-Alt-f · Alt-y      | toggle files drawer (bundled yazi, bottom)               |
| Alt-g · Alt-e · Alt-/   | lazygit · `$EDITOR` · git diff                           |
| Alt-1..9 / Ctrl-1..9    | jump to worktree N / workspace N (sidebar order)         |
| Ctrl-Alt-1..9           | launch / focus pinned programs (`[[pins]]`)              |
| Ctrl-Alt-↑/↓            | reorder the selected workspace / worktree                |
| Ctrl-Alt-/ · Ctrl-/     | search pane history · search across panes                |
| Ctrl-Alt-z              | zoom the focused pane / zone                             |
| Alt-r                   | time-travel replay of the focused pane (`[replay]`)      |
| Ctrl-g                  | keybind lock (pass every chord through to the pane)      |
| Alt-x / Alt-X           | close (pane if split, else tab) / remove worktree        |
| Ctrl-q                  | quit                                                     |

The defaults follow one modifier grammar, so a chord is predictable from its
modifiers: **Ctrl** moves focus only (and never creates/destroys, so `Ctrl-w`
stays free for the shell's delete-word); **Alt** is object lifecycle + tools
(create is `Alt-<letter>`); **Alt-Shift** is one level up (`Alt-w` worktree →
`Alt-W` workspace; `Alt-x` close → `Alt-X` remove worktree); **Ctrl-Alt** is
chrome toggles.

_The above is the `Normal` mode. Native `VimNormal` (with a `Space` leader
layer) and `Emacs` presets ship too; switch with `Ctrl-Alt-v` / `Ctrl-Alt-e`
(`Ctrl-Alt-n` back to Normal). `keymap_preset = "vscode"|"jetbrains"` overlays
familiar IDE chords._

## Install

### Nix / home-manager (recommended)

```nix
# flake.nix inputs
thegn.url = "github:blakeashleyjr/thegn";

# home-manager config
imports = [ inputs.thegn.homeManagerModules.default ];
programs.thegn = {
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

Or just the binary: `nix profile install github:blakeashleyjr/thegn`.

### nix-darwin (macOS)

The home-manager module above works on darwin unchanged. nix-darwin has no
per-user config-file mechanism, so `darwinModules.default` only puts the binary
on PATH system-wide — compose the two and let home-manager own the config:

```nix
darwinConfigurations."mac" = darwin.lib.darwinSystem {
  system = "aarch64-darwin";
  modules = [
    inputs.thegn.darwinModules.default
    { programs.thegn.enable = true; }        # environment.systemPackages

    home-manager.darwinModules.home-manager
    {
      home-manager.users.you = {
        imports = [ inputs.thegn.homeManagerModules.default ];
        programs.thegn = { enable = true; themeAccent = "#f083ba"; };
      };
    }
  ];
};
```

Enabling both is fine — they install the same store path. Apple silicon only:
the pinned nixpkgs has dropped `x86_64-darwin`, so the flake declares
`aarch64-darwin`, `x86_64-linux` and `aarch64-linux`.

### Prebuilt binary (no Nix)

Each tagged release attaches a `thegn` binary for **x86_64 Linux (gnu + musl)**
and **Apple silicon (aarch64-apple-darwin)** to the [releases page][releases].
Download the archive, verify the `.sha256`, and drop `thegn` on your `PATH`.
Windows binaries are not published yet.

**On macOS, prefer Homebrew or Nix.** The archives are deliberately unsigned, and
a tarball downloaded through a browser is quarantined by Gatekeeper — it needs
`xattr -dr com.apple.quarantine ./thegn` before it will launch. Homebrew formula
downloads, Nix store paths, and the locally generated `thegn.app` are not
quarantined, so none of them hit that. The reasoning is in
[`RELEASING.md`](RELEASING.md), which also covers the release process and the
crates.io / `cargo binstall` status.

[releases]: https://github.com/blakeashleyjr/thegn/releases

### Standalone

```sh
./install.sh    # builds thegn; installs tg, tg-tui, thegn + an owl app-launcher entry
tg              # thegn in the current terminal
tg --standalone # dedicated Alacritty window with the bundled profile (also: tg -s)
tg-tui          # same as plain `tg` — always the current terminal
```

`./install.sh` needs Rust/Cargo. `tg` and `tg-tui` run directly in the current
terminal, whatever it is; `tg --standalone` (`-s`) opens thegn in its own
Alacritty window using the bundled hermetic profile (`config/alacritty.toml`),
so Alacritty is only needed for that path. The installer also registers thegn
with your launcher, detecting which kind you have: a `thegn.desktop` entry with
the owl icon on Linux/BSD (Exec `tg --standalone`; searchable/pinnable in
GNOME/KDE/rofi/wofi), or a `thegn.app` bundle on macOS (see below).
thegn shells out to `git` (and `gh`/`ssh` as fallbacks where native support has
gaps); `lazygit` is optional.

### macOS app launcher

thegn is a TUI, so on macOS the launcher entry is a generated `thegn.app` in
`~/Applications` that opens a terminal emulator running thegn — Spotlight,
Raycast, Alfred and the Dock all index it. `./install.sh` writes it
automatically; for the Nix or Homebrew installs, which never run `install.sh`,
generate it from a checkout:

```sh
just macos-app                                  # points at `thegn` on PATH
just macos-app "$(command -v thegn)" /Applications
./packaging/macos/make-app.sh --help            # the generator's own options
```

It picks the first terminal it finds — Ghostty, WezTerm, kitty, Alacritty, then
Terminal.app — and runs thegn through a login shell so the tools it shells out
to are on `PATH`. Pin one with `--terminal /path/to/binary`. The bundle is
generated on your machine rather than downloaded, so it carries no
`com.apple.quarantine` xattr and Gatekeeper does not block it; a shipped
prebuilt `.app` would need Developer ID signing and notarization first. Re-run
after moving or reinstalling the binary (the launcher falls back to searching
the usual prefixes, but the baked path is what it prefers).

**macOS and Windows are unvalidated in this release.** Neither has been run
through a full interactive checklist. Windows compiles on msvc and passes its
IPC and Job-Object tests; macOS builds, tests and runs on Apple silicon, but its
CI job has never been run and no binaries are published (see
[`KNOWN_ISSUES.md`](KNOWN_ISSUES.md)). Treat both as work-in-progress:

- **macOS:** `./setup-macos.sh` checks every prerequisite (Xcode CLT, Nix or
  rustup + Homebrew deps) and offers to install what's missing, then builds.
  Nothing is installed without asking.
- **Windows (native, no WSL):** with [rustup](https://rustup.rs) + the VS Build
  Tools installed, `cargo install --path crates/thegn-host`. Run it inside
  [Windows Terminal](https://aka.ms/terminal). Container sandboxing is a
  Linux/WSL2 feature — native panes run host-side, scoped by Job Objects. See
  CONTRIBUTING "Windows (native) notes" for details.

Reports (and fixes) from either platform are welcome — that is how they graduate.

## How it works

- **A small Rust workspace.** `thegn-core` (substrate-agnostic domain logic:
  layered config, SQLite, keymap registry, theme, sandbox backends),
  `thegn-svc` (service seams with graceful degradation: gix-native git reads
  with CLI fallback, GitHub via octocrab/`gh`, SSH via the `ssh` CLI), and
  `thegn-host` (the compositor: tokio, portable-pty panes, termwiz
  diff-flush rendering, in-process chrome, and the pane daemon).
- **Fully event-driven.** The loop blocks on terminal input with no tick or
  timeout; PTY readers, hydration, and fs-watchers wake it over channels. A
  damage-region render planner repaints only what changed — pane output costs
  a bounded diff, not a chrome recompose. Idle CPU is ~0% by contract.
- **State.** SQLite at `$XDG_STATE_HOME/thegn/thegn.db` (WAL): repos,
  workspaces, worktrees, PR cache, layouts, session state. **git is the source
  of truth** for worktrees; the DB is a cache + resurrection layer. Restarting
  restores your exact workspace/worktree/pane position.
- **Worktrees** default to `~/.thegn/worktrees/<repo>/<branch-slug>`;
  `worktree_mode = "in_repo"` uses `<repo>/.worktrees`.

### CLI

Bare `thegn` launches the compositor; subcommands run non-interactively:
`pr`, `issue`, `ci`, `diff`, `list`, `integrate` (drain the local merge
queue), `disk` / `clean` (per-worktree disk usage / reclaim `target/`),
`repos`, `recent`, `config`, `env` (named execution environments), `theme`,
`share`, `forward`, `notify`, `logs`, `doctor`. `--profile <name>`
runs everything under a separate whole-process profile (own state/DB/config).

`thegn mcp serve` runs thegn as a read-only **MCP server** so a coding agent can
learn how thegn works — search/read the built-in help, the effective keymap, the
config reference, and your current (secret-redacted) config. Register it with
`claude mcp add thegn -- thegn mcp serve` (see [`docs/cli.md`](docs/cli.md)).

## Config

Behavior is configured in `~/.config/thegn/config.toml` — see
[`config/config.toml.example`](config/config.toml.example) for every key,
documented. Home-manager users configure via `programs.thegn.*`. A
repo-root `.thegn.{toml,yaml,yml,json}` overlays per-repo settings
(sandbox, keybinds, env selection). Highlights:

- `[theme]` — `accent` recolors every surface; named theme presets cycle with
  `Ctrl-Alt-t`.
- `[keybinds]` (+ `[keybinds.vim_normal]`, `[keybinds.emacs]`) and
  `[[actions]]` — rebind anything; define shell or composite custom actions.
- `[[agents]]` / `[[tools]]` — the "what to run" picker entries for new
  worktrees, each with an optional `harness`, `model`, `env` overlay (pin an
  account) and headless `permissions`; `[[pipeline.stages]]` overrides them
  per stage.
- `[metrics]` — Prometheus `/metrics` endpoints to scrape; target health and
  allowlisted values render in the chrome, no Prometheus server needed.
- `[merge_queue]`, `[pr_queue]`, `[share]`, `[forward]`, `[media]`,
  `[replay]`, `[lifecycle]` — the optional feature groups.

## Terminal compatibility

thegn renders to whatever terminal it runs in and **degrades gracefully** —
from bare shells (Linux console, plain `xterm`, `screen`/`tmux`, CI capture,
anything honoring `NO_COLOR`) up to fully-featured emulators (ghostty,
wezterm, kitty, foot, …). Color quantizes truecolor → 256 → 16 → mono;
Unicode box-drawing, dots, and the wordmark fall back to ASCII; undercurl and
mouse are enabled per terminal. A bounded startup probe (Device Attributes +
XTVERSION) catches modern emulators behind `ssh`/`tmux` that report a generic
`$TERM`. Override in `[theme]`:

```toml
[theme]
color  = "auto"   # auto | truecolor | 256 | 16 | none/mono   (THEGN_THEME_COLOR)
glyphs = "auto"   # auto | unicode | ascii                    (THEGN_THEME_GLYPHS)
```

Run **`thegn doctor`** (add `--json` for scripts) to see the detected
environment and exactly which features are enabled vs degraded.

## Sandboxing worktrees

By default each worktree's interactive process (agent / shell / tools) runs
inside a container or sandbox, so a coding agent can't reach your whole
machine. The worktree stays on the host and is **bind-mounted at its real
path**, so host-side git reads (sidebar, panel, PR) keep working.

- **Backends** (`[sandbox] backend`): `auto` walks `backend_chain` —
  `podman` (rootless, preferred) → `docker` → `bwrap` (lightweight, reuses
  host tools, no image) → host. `systemd`, `apple` (macOS `container`), and
  `wsl` are also selectable. Hardening presets: `open` / `hardened` (default)
  / `sealed` (no network, read-only root, all caps dropped).
- **Named environments** (`[env.<name>]`) place a worktree locally, over
  `ssh`, on Kubernetes, or on a managed provider; `thegn env` inspects and
  selects them. _(Non-local placement is **dev channel only** — see Channels.)_
- **Remote** (`[sandbox.remote]`) runs worktrees on another machine — the
  interactive pane over **mosh** by default, git reads and lifecycle over
  **ssh**; composes with the container backends. _(**Dev channel only.**)_
- **Opt out** — `backend = "none"` (or `enabled = false`). Agents need
  network and credentials, so `network` defaults to `nat` and
  `env_passthrough` forwards `SSH_AUTH_SOCK`/tokens.

## Channels

The **stable** build — what you get from a release binary, `nix profile install`,
or `./install.sh` — ships the shell described above. A set of experimental
subsystems is compiled in but **clamped off** outside the dev channel:

- remote worktrees over SSH and named non-local environments,
- cloud execution providers,
- the Observe dashboards and the placement engine,
- non-GitHub issue trackers (GitHub PR/issue viewing stays available).

Enable them with `THEGN_CHANNEL=dev`, or install the dev build
(`nix run .#dev` / `nix profile install .#dev`, which lands as `thegn-dev`
alongside a stable install). `thegn doctor` prints which channel you are on and
what is clamped.

## Development

New contributor? Start with [`CONTRIBUTING.md`](CONTRIBUTING.md) —
prerequisites per platform (Linux + macOS), quick start, and the dev loop.
`just doctor` diagnoses a broken dev environment. How thegn is put together,
and the gate behind each invariant, is [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md);
recipes for adding a config key, action, provider or plugin are in
[`docs/extending/`](docs/extending/README.md).

Run inside `nix develop` (rust toolchain + tools).

```sh
just quick [crate]   # inner loop: clippy on lib/bin only (no tests)
cargo nextest run -p <crate> <filter>  # targeted tests while iterating
just build           # one-time workspace debug build
just test            # pre-push correctness gate
just smoke           # pre-push correctness gate
just lint            # deliberate pre-PR/diagnostic gate
just start name=dev  # run the host with an isolated XDG_STATE_HOME
just ci              # final CI gate (coverage; no `just e2e` snapshot suite)
```

Iterate with the scoped commands above; save the heavy gates (`just test`,
`just lint`, `just coverage`, `just ci`) for deliberate pre-push, diagnostic, or
pre-PR validation. For UI work, run the full `just e2e` suite once the change is
settled. See
[`docs/coverage.md`](docs/coverage.md) for the tier breakdown, and
[`docs/testing-with-muse.md`](docs/testing-with-muse.md) for driving and
testing the TUI itself — by hand, in CI, or from an agent.

Contributor docs: [`CLAUDE.md`](CLAUDE.md) (architecture + invariants),
[`tasks.md`](tasks.md) (roadmap), `openspec/specs/` (behavior specs),
`docs/superpowers/{plans,specs}/` (design docs per feature).

## Security

Please report vulnerabilities **privately** — see
[`SECURITY.md`](SECURITY.md) for the process. Reports about sandbox escape,
credential leakage between worktrees, or command injection through untrusted
repo content are especially valued.

## License

Dual-licensed under either

- [MIT](LICENSE-MIT) ([summary](https://opensource.org/licenses/MIT)), or
- [Apache License 2.0](LICENSE-APACHE) ([summary](https://opensource.org/licenses/Apache-2.0))

at your option.

Unless you state otherwise, any contribution you intentionally submit for
inclusion in thegn is dual-licensed under those same terms, with no additional
terms or conditions (per Apache-2.0 §5).
