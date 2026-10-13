# Rework shell completions — the packager delivers, the binary answers, and a `<TAB>` gets a contract

> **SUPERSEDES `openspec/changes/package-shell-completions`.** That change asked
> the same question (THE-36) and got the delivery half right: nothing installs
> completions, so `nix/package.nix` should. What it got wrong is the half it
> called a non-goal — it recommended documenting the industry-default
> `eval "$(thegn completions bash)"` for tarball users, which is the one pattern
> thegn specifically must not recommend (it spawns a shell per pane), and it
> deferred live values as "a different feature". Live values are the whole point
> of completing a noun-verb CLI over a live state DB, and the mechanism that
> delivers them (`clap_complete`'s registration shim) is also what makes a
> statically installed file un-stale-able. One mechanism, not two. Archive
> `package-shell-completions` in place; its packaging contract survives here,
> strengthened.

## Why

**Nothing installs completions on any install path.** `thegn completions
<shell>` has existed since `add-cli-namespaces-and-remote-open` and is a good
generator — the live clap tree, named from the invoked binary so `thegn` and
`tg` each get a correct script — but `nix/package.nix` never calls it,
`release.yml` ships only the bare binary, and the docs are one line in
`docs/cli.md` and one in `docs/help/cli.md`. So a CLI with ~40 verbs completes
nothing for anyone unless they find the verb and wire their shell by hand.

**The contract describes only a generator.** The `cli` spec's completions
requirement is one sentence and one bash scenario. It says nothing about who
installs the output, what a `<TAB>` may cost, or what a `<TAB>` may never do —
and those last two are the load-bearing parts. A completion request is a
**process launch on a keypress**; the existing `run_subcommand` path resolves
the channel, loads the layered config, **opens the DB read-write**, installs a
log subscriber and publishes a cgroup policy before it dispatches. If "a `<TAB>`
does not do any of that" is not in the spec, it will regress.

**The obvious install instruction is the wrong one _here_.** The reference guide
that seeded THE-36, and `gh`/`rustup`/`kubectl`, all recommend
`eval "$(tool completion zsh)"` in the user's rc: never stale, at the cost of
one process spawn per shell launch. thegn is a terminal multiplexer. It spawns a
shell **per pane**, and a warm reattach restores every pane in a session at
once — so that instruction would put a config-loading, DB-opening `thegn` launch
into every pane thegn itself opens, against a stated sub-300 ms / 0%-idle
invariant. `gh` does not have this problem because `gh` is not the thing
spawning your shells.

## What Changes

1. **Delivery moves to the packager.** `nix/package.nix` generates and installs
   completion files for **both** installed names (the channel binary and its
   short alias) for bash, zsh and fish into the standard share dirs, guarded by
   `canExecute` so cross builds skip rather than fail; `release.yml` ships one
   arch-independent completions asset (the CLI tree has no `#[cfg]`-gated
   `Command` variants, so every target emits byte-identical scripts). The user's
   shell rc does **nothing**. `just completions` is the local convenience, not a
   gate.
2. **The installed artifact is a registration shim, not a static script.**
   `thegn completions <shell>` now emits `clap_complete`'s `CompleteEnv`
   registration, whose body calls back into the binary on every `<TAB>`. Startup
   cost: zero (a file, autoloaded lazily by zsh's `fpath` and
   bash-completion's dynamic loader). Staleness: structurally impossible — the
   shim contains no command names, it asks the binary. Values: live.
   `--static` keeps emitting the self-contained `aot` script as the documented
   degradation path.
3. **A `<TAB>` gets a hard contract.** Dispatch from the top of `main()`, never
   through `run_subcommand`; one `env::var_os` when not completing; read-only DB
   with a short busy timeout so a `<TAB>` on a machine that has never run thegn
   creates **nothing**; no network and no forge I/O, ever; fail open — any
   error, timeout or panic exits 0 having printed nothing, and the shell falls
   back to filename completion; `--profile` honoured via a raw-argv scan.
4. **Value sources are a seam.** `thegn_core::completion::CATALOG` maps
   `(command path, arg id) → SourceKind`, drift-guarded against the live clap
   tree by a shrink-only ratchet. DB-, config-, capability- and keymap-derived
   kinds are implemented; `branch` (git I/O) and `pr`/`issue` (network) are
   `reserved` with the reason recorded, because a `<TAB>` may not pay for
   either.
5. **Staleness becomes diagnosable.** `thegn doctor` reports, per shell and per
   command name, where completion files are installed and whether they are
   `fresh` / `stale` / `dynamic` / `absent`, printing the exact fix command for
   the ones that need one. Detection instead of ceremony — the few hand-installed
   users are told once, rather than every user paying at every shell launch.
6. **The docs say all of this**, including the one sentence a reader coming from
   `gh` needs: do not put `eval "$(thegn completions zsh)"` in your rc, and why.

## Non-goals

- **A third-party completion framework** (carapace and friends). A second vendor
  to track against the one-source rule, for a problem the binary already answers.
- **Completing inside the TUI's command palette.** A different surface with its
  own spec.
- **A Homebrew formula.** The release asset this change adds is what a formula
  would consume; the formula lives outside this repo.
- **Windows `$PROFILE` installation.** The PowerShell script is generated and
  shipped; wiring it into a profile is a packaging question the Windows port
  (AX) owns. Same for elvish, which has no standard location either.
- **Branch, PR and issue completion.** Deliberately reserved, not forgotten: a
  `<TAB>` does not run git and does not call the forge.

## Impact

- **Roadmap:** Wave 3+ — `completions/config/docs (THE-36/38/4)`
  (`tasks.md:185`); **A 6** (CLI surface v2, which created the completions verb
  under `add-cli-namespaces-and-remote-open`); **AO 494/495** (single-command
  install, NixOS/home-manager — both are install paths this makes complete).
- **Linear:** THE-36.
- **Specs:** `cli` — 1 MODIFIED ("Shell completions are generated from the CLI
  definition"), 4 ADDED (packager delivery; the `<TAB>` fast-path contract;
  value sources implemented-or-reserved; freshness diagnosable).
- **Code:** `nix/package.nix`, `.github/workflows/release.yml`, `justfile`;
  `thegn_core::completion` (new, 95%-gated, not in `cov_ignore`);
  `thegn-host/src/complete.rs` (new) + ~5 lines of `main.rs`;
  `thegn-host/src/completions_health.rs` (new) + a `doctor` section;
  `test/smoke.sh`; `test/completion-slot-ratchet.txt` (new, shrink-only).
- **New gates:** `completion_slots_are_bound_or_pinned` (slot drift over the
  live clap tree), `clap_complete_is_imported_once` (the unstable-API
  containment boundary), and a smoke assertion that a `<TAB>` against an empty
  state root creates no files.
- **No capability-catalog row.** Completion is a local generator, like `--help`,
  not an external door into a running instance; `SURFACE_GAPS` is unaffected. It
  does _consume_ the catalog — `thegn api call <TAB>` completes from `CATALOG`
  ids.
- **No new action, keybind, zone or panel section**, so the help ratchets are
  unaffected; `docs/help/cli.md` already claims this surface and gains prose
  only.
- **No DB schema change** — the completion path opens the existing DB read-only.
- **Dependency risk, stated:** two explicitly-unstable Cargo features
  (`clap_complete/unstable-dynamic`, `clap/unstable-ext`), contained to one file
  and with a specified fallback. See design.md.
