# Chunk 2 — done: the engine (live values at `<TAB>`)

Issue THE-36. Branch `tg/the-36-completions`. **Plan A** (see below).

## Step 0 — the decision gate

**Plan A.** Both explicitly-unstable features build and behave against the
locked versions (`clap` 4.6.6 / `clap_complete` 4.6.9):

- `clap_complete/unstable-dynamic` — gates `engine` (candidate protocol,
  `ArgValueCompleter`) and `env` (`CompleteEnv`: the shim + the
  `COMPLETE=<shell>` callback). It already implies `clap/unstable-ext`, which
  is nonetheless declared explicitly so a reader sees why it is on.
- `clap/unstable-ext` — gates `Arg::add`.

Verification before enabling: a standalone five-line spike compiled, emitted a
zsh registration shim, and answered a decorated argument's `<TAB>` with a live
candidate plus its help text. `cargo check -p thegn-host` green with both on.
One transitive dep added (`is_executable` 1.0.6); `shlex` and `clap_lex` were
already in the lock.

Containment: `crates/thegn-host/src/complete.rs` is the only file importing
`clap_complete::{engine,env}`, asserted by `clap_complete_is_imported_once`.
Both Cargo.toml sites carry a comment naming what the feature gates and where
the boundary is.

## Commits (4, on the branch)

| commit        | what                                                                                           |
| ------------- | ---------------------------------------------------------------------------------------------- |
| `098941fd`    | `build(completions)` — the feature flags + lock, with the decision recorded                    |
| `68d35a51`    | `feat(completions)` — `thegn_core::completion` + `complete.rs` + `main.rs` + the drift ratchet |
| `6caf8b08`    | `test(completions)` — the smoke block                                                          |
| _(this file)_ | the summary                                                                                    |

## What landed

**`thegn_core::completion`** (new; pure, no new dependency, not in `cov_ignore`):

- `catalog.rs` — `CATALOG: &[Slot]`, `(command path, arg id) → SourceKind`, in
  the implemented-or-`reserved` seam idiom. `Reserved(Reserved::{Branch,Pr,
Issue})` each carry their reason (git I/O / network). `Structural` is a
  declared decision. `SourceKind::ALL` + `kind()` + `is_implemented()` +
  `reads_db()`/`reads_config()`, with the `kind_coverage`-shaped test.
- `candidate.rs` — `Candidate`, and `refine`: sanitise → byte-prefix match →
  stable first-wins de-dup → description sanitise → cap at 200.
- `sources.rs` — the `CompletionSource` trait plus `DbSource` (read-only DB),
  `ConfigSource` (pure over `&Config`), `StaticSource` (in-process catalogs).
- `mod.rs` — `Deadline` (injected-`Instant` arithmetic, `THEGN_COMPLETE_BUDGET_MS`,
  default 100 ms, no watchdog thread).

**`thegn-host/src/complete.rs`** (new) — `maybe_complete()` dispatched from the
top of `main()` between `scrub_git_env` and `install_panic_hook`; the tree
decorator; `write_registration` for `thegn completions <shell>`.

**`main.rs`** — the module decl, the early dispatch (with the ordering comment),
and `Completions { shell, --static }`.

## Two decisions the chunk left to the coder

**Shell-hostile values are DROPPED, not escaped** (documented in
`candidate.rs`'s module doc, tested in
`refine_drops_hostile_values_but_keeps_the_rest`). Every shell protocol here is
line-oriented with an in-line separator (`value\tdesc` PowerShell,
`value:desc` zsh, bare lines bash/fish), so a value carrying a newline or tab
does not render badly — it **desynchronises the parse**, turning one candidate
into two. No escaping works across all five shells. Descriptions are the
opposite call: sanitised (control chars → space, collapsed, truncated), because
losing a description must never lose the value it describes.

**The shim invokes `thegn` by NAME, not by path.** `CompleteEnv`'s default
completer is `args_os()[0]`, i.e. absolute — and chunk 1 generates the shipped
scripts inside a Nix build sandbox (`$out/bin/thegn`) and a CI temp dir
(`$scratch/thegn`). Baking either in would ship a release asset that calls a
path no user has. `.bin(name).completer(name)` resolves through PATH, which is
the only way the user could have typed the command anyway. Smoke asserts it.

## Two clap traps, both found by tests, both fixed and commented

1. **`Command::mut_arg` on an already-BUILT tree corrupts the key index.**
   `MKeyMap` holds a long/short → index map that `remove_by_name` + `push`
   invalidates. Symptom: `thegn --profile work wt rm x` parsed as
   `--version`. Fix: decorate **before** `cli_help::attach` (which builds).
2. **`mut_arg` also reorders, and clap numbers positionals by list order.**
   Decorating `env set`'s optional `worktree_pos` before its required `name`
   swapped their indices and tripped clap's own debug assert. Fix: `mut_args`,
   which maps in place.

Neither is reachable from the old static path, which is why they were latent.

## Gates added

- `completion_slots_are_bound_or_pinned` — walks the live clap tree; every
  value-taking argument must be in `CATALOG` or pinned in
  `test/completion-slot-ratchet.txt`. Shrink-only, and it also rejects stale
  pins and slots that are in both. Regenerate with the `#[ignore]`d
  `update_completion_slot_ratchet`.
  - **Seeded at 159 entries** of 288 real slots. Per the chunk, the tail is
    deliberately not chased here.
  - clap's four `global = true` args (`--config`, `--log-level`, `--set`,
    `--profile`) are counted once at the root instead of once per command
    path. Without that the file was **1246 lines** — four decisions dressed up
    as a thousand.
- `decoration_does_not_change_parsing`, `every_implemented_catalog_slot_actually_binds`
  (catches a stale catalog row, which the drift test cannot),
  `clap_complete_is_imported_once`, the `--profile` argv-scan table.
- Smoke: per-shell registration markers, `--static` still `aot`, **a `<TAB>`
  against an empty `XDG_STATE_HOME` exits 0 / prints nothing / leaves the dir
  empty**, live worktrees + repos + capability ids + config keys, prefix
  filtering, an exhausted budget completing nothing quietly, and a 300 ms
  canary (commented as a canary, not a perf gate).

## Verified by hand (debug build)

```
thegn wt rm <TAB>            /wt/alpha:tg/alpha … + tg/alpha:/wt/alpha
thegn open <TAB>             alpha:/code/alpha … + /code/alpha:alpha
thegn api call worktrees.    worktrees.list / .open / .create + summaries
thegn config set theme.acc   theme.accent
thegn --set theme.acc        theme.accent
thegn wt new x --env <TAB>   docker, nix           (config-derived)
thegn --profile <TAB>        personal, work        (config-derived)
thegn session open --agent   claude:claude --x     (config-derived)
thegn mcp install <TAB>      git                   (config-derived)
thegn completions <TAB>      bash elvish fish powershell zsh (clap, structural)
```

- **Empty `XDG_STATE_HOME`: still empty afterwards, exit 0**, and structure
  still completes.
- `thegn --profile work wt rm /wt/<TAB>` reads the **work** profile's DB; the
  default profile's does not see that worktree.
- `THEGN_COMPLETE_BUDGET_MS=1` → nothing, silently.
- ~42–55 ms per request on a **debug** build (release will be lower); the smoke
  canary is 300 ms.

## Known / accepted

- **`--profile <name>` does create one directory.** `profile::reroot` `mkdir
-p`s the named profile's state dir. Without it a `<TAB>` would read the
  _shared_ DB and offer another profile's worktrees, which is worse than an
  empty directory. The default profile — every completion that does not name
  one — creates nothing, and that is what smoke asserts. Documented in
  `complete.rs`'s module doc.
- **Unmigrated state roots** get structural completions only until the next
  real `thegn` run (`run_startup_migration` is skipped, as the design accepts).
- **Four implemented kinds have no slot yet** — `theme`, `tool`, `plugin`,
  `action`. Each is served and tested, but nothing in today's CLI grammar takes
  one, so they wait for a verb rather than being bound to an approximation.
  Noted in the `CATALOG` doc comment.
- **`--config <path>` is not honoured on the `<TAB>` path.** Parsing our own
  argv for it would change only which `[[agents]]` names appear. Commented.
- **`sandbox prune --host`** takes a `[host.<name>]` _config_ key, not a row
  from the `hosts` table, so it is pinned rather than bound to `SourceKind::Host`.
  Same for `host add name` (it names a NEW host) and `env create name`.

## Out of scope, worth doing next (as the chunk asked me to note)

`completions` still dispatches through `run_subcommand`, which resolves the
channel, loads the layered config and **opens the DB** before it reaches the
generator. That is now avoidable — and it is precisely why chunk 1's
`nix/package.nix` and `release.yml` both have to redirect the whole XDG surface
to a scratch dir just to generate a script.

## Files touched

`Cargo.toml`, `Cargo.lock`, `crates/thegn-core/src/lib.rs`,
`crates/thegn-core/src/completion/{mod,catalog,candidate,sources}.rs`,
`crates/thegn-host/src/complete.rs`, `crates/thegn-host/src/main.rs`,
`test/smoke.sh`, `test/completion-slot-ratchet.txt`.

Nothing under `nix/`, `docs/`, `openspec/`, `.github/`, or
`crates/thegn-host/src/cmd/doctor.rs` — those belong to chunks 1, 3 and 4.
`crates/thegn-host/Cargo.toml` needed no change: it already inherits
`clap_complete.workspace = true`.
