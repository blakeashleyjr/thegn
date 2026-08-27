# Tasks — rework-shell-completions

Four groups, one per chunk of the implementation, plus validation. Chunks 1, 3
and 4 are independent of each other and of chunk 2 in both directions; chunk 2 is
the one Rust vertical (core policy _and_ host wiring together, so no coder waits
on another's API). Chunks 1 + 3 + 4 alone are a coherent improvement: every
install path gains working structural completions, the contract and docs match
reality, and staleness becomes detectable. Chunk 2 is what adds live values.

State below is as of this branch's tip (`tg/the-36-completions`).

## 1. Delivery — the packager installs the file (chunk 1)

- [x] 1.1 `nix/package.nix`: `installShellFiles` in `nativeBuildInputs` and a
      `postInstall` loop generating bash/zsh/fish for **both** installed names
      (channel binary + short alias), via the alias symlink so each script
      targets the command the user types. Ordering is load-bearing: after the
      alias symlink, before `wrapProgram`.
- [x] 1.2 Guard the loop with a `canExecute` check on the build vs host
      platform — a cross build skips completions rather than failing.
- [x] 1.3 Point `HOME`/`XDG_STATE_HOME`/`XDG_CONFIG_HOME` at scratch and skip the
      brand migration inside the sandbox: the generator still reaches
      `run_subcommand`, which loads config and opens the DB on the way.
      Generate to files, never through process substitution, which would hide a
      non-zero exit and install a truncated script as a success.
- [x] 1.4 `.github/workflows/release.yml`: one arch-independent completions
      asset (`share/`-shaped for bash/zsh/fish, plus a plain dir each for elvish
      and PowerShell, both names, `.sha256` beside it, `--clobber` for re-run
      safety).
- [x] 1.5 `just completions` — the local convenience that writes both names for
      all five shells under `target/`, deliberately **not** in `just ci`: it is
      not a gate.
- [x] 1.6 Verify no home-manager / darwin change is needed (the HM zsh module
      already appends each profile's `share/zsh/site-functions`;
      `XDG_DATA_DIRS` carries the HM and `.nix-profile` share dirs).
- [ ] 1.7 Build-verify the outputs: `nix build .#default` and `.#dev`, then check
      `result/share/zsh/site-functions/_tg` exists and registers `tg` (the
      alias-name proof), and that the dev channel's files follow the dev binary
      names with no collision against a stable install beside them.

## 2. The engine — live values at `<TAB>` (chunk 2)

- [x] 2.1 Decision gate: confirm `clap_complete/unstable-dynamic` +
      `clap/unstable-ext` build and behave against the locked versions before
      committing to Plan A; record the verification and the containment boundary
      in the Cargo.toml comments.
- [x] 2.2 `thegn_core::completion::catalog` — `CATALOG: &[Slot]`,
      `(command path, arg id) → SourceKind`, implemented-or-`reserved` with the
      reason carried in the enum, `Structural` as a declared decision.
      **Unit tests**: kind coverage, `reads_db`/`reads_config` classification.
- [x] 2.3 `thegn_core::completion::candidate` — sanitise → byte-prefix match →
      stable first-wins de-dup → description sanitise → cap. **Unit tests**
      including "hostile values are dropped, the rest survive".
- [x] 2.4 `thegn_core::completion::sources` — the object-safe
      `CompletionSource` trait plus the DB (read-only), config (pure over
      `&Config`) and in-process-catalog implementations. **Unit tests** per
      source.
- [x] 2.5 `Deadline` (injected `Instant`, `THEGN_COMPLETE_BUDGET_MS`, default
      100 ms, no watchdog thread). **Unit tests** for the boundary cases.
      The module is **not** added to `cov_ignore`; the 95%-line core gate applies.
- [x] 2.6 `thegn-host/src/complete.rs` — `maybe_complete()` dispatched from the
      top of `main()`, the tree decorator, and the registration writer. The only
      file importing `clap_complete::{engine,env}`.
- [x] 2.7 `main.rs` — module decl, the early dispatch with its ordering comment,
      and `completions <shell> [--static]`. Nothing else.
- [x] 2.8 `--profile` raw-argv scan + reroot before serving, so a completion
      under an explicit profile reads that profile's DB. **Unit tests** over the
      argv table.
- [x] 2.9 Gates: `completion_slots_are_bound_or_pinned` (drift over the live
      clap tree, shrink-only `test/completion-slot-ratchet.txt`, rejects stale
      pins and double-listed slots), `clap_complete_is_imported_once`,
      `decoration_does_not_change_parsing`,
      `every_implemented_catalog_slot_actually_binds`.
- [x] 2.10 `test/smoke.sh`: a registration marker per shell; the shim calls the
      binary by name, not by the build path it was generated from; `--static`
      still emits an `aot` script; **a `<TAB>` against an empty state root exits
      0, prints nothing and leaves the directory empty**; live worktrees, repos,
      capability ids and config keys; prefix filtering; an exhausted budget
      completing nothing quietly; the 300 ms canary.

## 3. Contract + docs (chunk 3)

- [x] 3.1 This change folder: proposal, design, tasks, and the `cli` delta —
      1 MODIFIED requirement, 4 ADDED.
- [x] 3.2 `docs/cli.md`: rewrite `## Completions` — packaged installs are done
      already; the one command per shell for everyone else, with the real
      destination path; **do not `eval` in your rc**, with the per-pane reason;
      staleness and how `thegn doctor` reports it; what completes and what is
      deliberately not offered.
- [x] 3.3 `docs/help/cli.md`: the in-app voice of the same, in two lines. No new
      action ids or keybinds are claimed, so the help ratchets are unaffected.
- [x] 3.4 `docs/extending/completion-source.md` + the `docs/extending/README.md`
      index row: the recipe for adding a value source, ending in its gates.
- [ ] 3.5 On completion of the implementation, `/opsx:sync` this delta into
      `openspec/specs/cli/spec.md` and archive both this change **and the
      superseded `package-shell-completions`**. Do not hand-edit the main spec.

## 4. Health — staleness is diagnosable (chunk 4)

- [ ] 4.1 `thegn-host/src/completions_health.rs`: `search_paths(env, exe)` over
      the per-shell standard locations for **both** command names, with the
      environment injected as a struct rather than read from the process.
      **Unit tests**: XDG overrides honoured and fall back correctly; the
      install prefix derived from a `…/bin/thegn` exe path; both names covered.
- [ ] 4.2 `classify(installed, generated) -> Fresh | Stale | Dynamic` — a body
      carrying the completion env-var marker is `Dynamic` (a shim asks the
      binary and therefore cannot go stale) and is never diffed; otherwise
      byte-equality after trailing-whitespace normalisation. **Unit tests** for
      each arm plus a tempdir round-trip. Never compare mtimes — Nix normalises
      store timestamps.
- [ ] 4.3 `cmd/doctor.rs`: a `Completions` section in the existing style — a
      row per (shell, command) with its state and path; `stale`/`absent` rows
      print the exact fix command with the destination filled in; `dynamic` rows
      say they cannot go stale; an all-healthy install collapses to one line.
      Keep the edit small — call into the module, do not inline it.
- [ ] 4.4 `thegn doctor --json`: the same data under a `completions` key
      (`{shell, command, state, path}`), through the existing JSON helper.
- [ ] 4.5 `test/smoke.sh` (the doctor block only): the section is printed, the
      JSON key parses, and doctor exits 0 with nothing installed anywhere.

## 5. Validation

- [ ] 5.1 Scoped while iterating (`CLAUDE.md` dev-loop policy):
      `just quick thegn-core`, `just quick thegn-host`,
      `cargo nextest run -p thegn-core completion`,
      `cargo nextest run -p thegn-host complete`,
      `cargo nextest run -p thegn-host help` (help ratchets),
      `openspec validate --all --strict`.
- [ ] 5.2 `just smoke` — the completions and doctor blocks.
- [ ] 5.3 Pre-PR gate, run **once** when the whole change is in: `just ci`
      (lint, coverage's 95% core gate over the new module, the cross/feature/MSRV
      checks that would catch an unstable-feature break, `nix-build` for the
      packaging half, and `openspec-validate`).

## Notes

- **No DB schema change**, so no `SCHEMA_VERSION` bump: the completion path
  opens the existing DB read-only and creates nothing.
- **No `ACTION_SPECS` entry, no keybind, no new help page.** `docs/help/cli.md`
  already claims this surface; the change adds prose only, so the action, prose
  and context help ratchets are untouched.
- **No capability-catalog row.** `completions` is a local generator like
  `--help`, not a door into a running instance; `SURFACE_GAPS` is unaffected.
- **e2e is not touched** — nothing here alters a frame. Do not re-record.
