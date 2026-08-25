# Install generated shell completions with the package

Linear: THE-36

## Why

THE-36 asks what the right layer for shell completions is (seeded by
agent-of-empires' shell-completions guide). The audit's answer: **the right
layer already exists and is the spec'd one** — `thegn completions <shell>`
generates bash/zsh/fish/elvish/powershell scripts from the live clap tree
(`cli` spec, "Shell completions are generated from the CLI definition"),
named for the invoked binary (`thegn` / `tg`), buffered against closed pipes.
That is the layer comparable tools converge on (agent-of-empires, `gh`,
`rustup`: a runtime verb as the single source of truth, so completions can
never go stale against the installed binary).

What's missing is that **nothing consumes it**. `nix/package.nix` installs the
binary, the `tg` symlink and the yazi shim — no completions. So every install
path ships a CLI with ~40 subcommands and zero tab-completion unless the user
discovers the verb (documented in one line of `docs/help/cli.md`) and wires
their shell by hand. Two further wrinkles the packaging layer must own:

- **The alias.** Completions register per command name. A `_thegn` file never
  fires for `tg` — the invoked-binary-name logic in the verb handles this
  (generate via the `tg` symlink and the script targets `tg`), but only if
  the package generates _both_.
- **The dev channel.** A dev build renames to `thegn-dev`/`tg-dev`; its
  completion files must follow the binary names or they collide with a
  stable install sitting beside it.

## What Changes

- **`nix/package.nix` `postInstall`** runs the just-built binary to generate
  completions for **both installed names** (`binName` and `aliasName` — via
  the symlink, so the script targets the right command) for bash, zsh and
  fish, into the standard share dirs
  (`share/bash-completion/completions/<name>`,
  `share/zsh/site-functions/_<name>`,
  `share/fish/vendor_completions.d/<name>.fish`). Guarded by
  `stdenv.buildPlatform.canExecute stdenv.hostPlatform` (cross builds skip,
  never fail), with a sandbox-safe `HOME`/`XDG_*` so the tolerant config load
  inside the binary touches nothing.
- **Docs for non-nix installs**: `docs/help/cli.md` (and the README install
  section) gain the per-shell one-liners — static file
  (`thegn completions zsh > ~/.zfunc/_thegn`) or eval-on-startup
  (`source <(thegn completions bash)`) — covering release-tarball users.
- **Spec**: the `cli` capability's completions requirement is MODIFIED to add
  the packaging contract (both names, standard share dirs, cross-build skip),
  gated by the `nix-build` step already in `just ci`.

## Non-goals

- **Completions inside release tarballs.** The archives are produced by a
  matrix of `taiki-e` builds; shipping per-shell files there means running
  the target binary on each runner and re-plumbing the archive layout for a
  user population that installs to arbitrary prefixes anyway. The documented
  eval/static one-liners serve them with zero staleness risk; revisit only if
  asked.
- **elvish/powershell packaging.** The verb keeps emitting them; the nix
  share-dir convention covers bash/zsh/fish, and home-manager/nix-darwin pick
  those up automatically. Windows packaging owns powershell when
  `add-windows-ci-distribution` gets there.
- **A home-manager option.** `programs.thegn` installs the package; shells
  with `enableCompletion` pick up the share dirs — nothing to add.
- **Dynamic (runtime-queried) completions** — completing worktree/repo names
  inside the shell. A real idea, but a different feature with a different
  cost (clap `CompleteEnv` or per-shell glue); not scoped here.

## Impact

- Roadmap: **A 6** (CLI surface v2 — completions listed under
  `add-cli-namespaces-and-remote-open`), **AO 494** (single-command install),
  **AO 495** (NixOS / home-manager module).
- Specs: `cli` — 1 MODIFIED ("Shell completions are generated from the CLI
  definition").
- Code: `nix/package.nix` only, plus `docs/help/cli.md` / `README.md` prose.
  No Rust changes; no new action ids or help pages (the existing `cli` help
  page already claims the surface), so the help ratchets are unaffected.
- In-flight reconciliation: `add-cli-namespaces-and-remote-open` created the
  completions requirement this change modifies (already synced to the main
  spec); its delta text is untouched — this change layers packaging on top.
- No capability-catalog row: `completions` is a local generator (like
  `--help`), not an external door into a running instance; `SURFACE_GAPS` and
  the catalog are unaffected.
