# Chunk 2 — The engine: live values at `<TAB>`

**Issue:** THE-36 (right layer for shell completions).
**Design:** `.thegn/pipeline/architect/design.md` — read §§3–6 before writing
code. §4 is a **contract**, not advice; every MUST in it is a done-criterion
here.

This is the one Rust vertical of the change. It deliberately contains both the
`thegn-core` policy module and the `thegn-host` wiring so that no other coder is
blocked on your API landing. It is also the only chunk that can be deferred
without stranding the others.

## Why

A static, clap-derived completion script can complete _structure_ — verbs,
flags, `ValueEnum` arguments — and nothing else. For a noun-verb CLI sitting on
top of a live state DB, that leaves all the value on the table:
`thegn wt rm <TAB>` cannot know your worktrees, `thegn open <TAB>` cannot know
your repos, `thegn attach <TAB>` cannot know the daemon's live sessions. Only a
process running at `<TAB>` time can answer those.

The design's shape: install a **registration shim** (chunk 1 delivers it) whose
body calls back into `thegn` on each `<TAB>`. Shell startup cost stays zero, the
structure can never go stale, and values are live. Your job is the callback and
the policy behind it.

## Files you own

- `crates/thegn-core/src/completion/` (new: `mod.rs`, `catalog.rs`,
  `candidate.rs`, `sources.rs`)
- `crates/thegn-core/src/lib.rs` (one `pub mod completion;` line)
- `crates/thegn-host/src/complete.rs` (new)
- `crates/thegn-host/src/main.rs` (≈5 lines: the early dispatch, a `--static`
  flag on the existing `Completions` variant, the module decl)
- `crates/thegn-host/Cargo.toml`, `Cargo.toml` (feature flags)
- `test/smoke.sh` (the completions block around line 519)
- `test/completion-slot-ratchet.txt` (new)

Do not touch `nix/`, `docs/`, `openspec/`, `.github/`, or
`crates/thegn-host/src/cmd/doctor.rs` — other chunks own those.

## Step 0 — decision gate (do this first, before any other work)

Plan A needs two **explicitly unstable** Cargo features:
`clap_complete/unstable-dynamic` (gates the `engine` and `env` modules) and
`clap/unstable-ext` (gates `Arg::add`, which is how a value completer attaches
to an argument). Both are verified present in the locked versions
(`clap_complete` 4.6.9, `clap_builder` 4.6.6) but both can break on a bump.

Enable them, write a five-line spike that calls
`clap_complete::env::CompleteEnv::with_factory(…).complete()` and attaches one
`ArgValueCompleter` to one argument, and run `just quick thegn-host`. Also run
`cargo check --workspace --all-features` and `cargo-1.89 check --workspace
--locked` (the `check-features` and `check-msrv` gates) — an unstable feature
that breaks either is a Plan B trigger.

- **Spike compiles and both gates pass ⇒ Plan A.** Continue below.
- **Otherwise ⇒ Plan B** (specified at the end of this file). Same core module,
  same contract, same protocol; only the shell plumbing is hand-written.

Record which plan you took, and why, in the commit message.

## Approach — Plan A

### 1. `thegn_core::completion` — pure policy

No new dependency. Nothing substrate-y. This module is **not** in the justfile's
`cov_ignore` regex and is therefore gated at **95% lines** by `just coverage` —
write the tests as you go and do **not** widen the regex.

**`catalog.rs` — the slot catalog.** A `const CATALOG: &[Slot]` where
`Slot { command_path: &str, arg_id: &str, source: SourceKind }`. `command_path`
is the space-joined path from the root (`"wt rm"`, `"api call"`, `""` for
top-level). This is the single source of truth for which slot takes which
values; the drift test (step 5) walks the live clap tree against it.

`SourceKind` is an implemented-or-`reserved` enum in the repo's seam idiom
(§5 of the design, and `docs/ARCHITECTURE.md` §5 for the pattern):

- implemented, DB-derived: `Worktree`, `Repo`, `Session`, `Host`
- implemented, config-derived: `Env`, `Profile`, `Theme`, `Agent`, `Tool`,
  `Plugin`, `McpServer`, `ConfigKey`
- implemented, in-process: `Capability` (from `thegn_core::capability::CATALOG`),
  `Action` (keymap ids)
- `Structural` — clap already completes it (subcommand names, flags,
  `ValueEnum` args such as `completions <shell>`); must be declared, not left
  unclassified
- `Reserved(&'static str)` — carrying the reason. v1: `Branch` ("git I/O the
  `<TAB>` path declines to pay for"), `Pr` and `Issue` ("network — a `<TAB>`
  must never make a forge call")

Give `SourceKind` an `is_implemented()` and a `kind()` string id, and unit-test
that every variant round-trips its id and that no id repeats — the same shape
the other seams' `kind_coverage` tests use.

**`candidate.rs` — candidate policy.** `Candidate { value: String, description:
Option<String> }` plus the pure filtering pipeline, all unit-tested:

- prefix match against the current word (byte-prefix, not fuzzy — shells expect
  prefix semantics);
- stable de-duplication by value, first occurrence wins;
- a hard cap (`MAX_CANDIDATES`, 200) — a shell that gets 5000 candidates is
  useless and slow to render;
- description truncation to a fixed width (say 60 columns) with an ellipsis at a
  char boundary — test with multi-byte input, this is the classic panic;
- escaping/rejection of shell-hostile values: a candidate containing a newline
  or a tab would corrupt the `value\tdescription` protocol, so drop or sanitise
  it (decide, document the choice in the module doc, and test it).

**Budget.** `Deadline { started, budget_ms }` with `expired()`, budget read from
`THEGN_COMPLETE_BUDGET_MS` (default 100). Pure arithmetic over an injected
`Instant`/duration so it is testable without sleeping. No watchdog thread.

**`sources.rs` — the source seam.** An object-safe trait:

```rust
pub trait CompletionSource: Send + Sync {
    fn kind(&self) -> SourceKind;
    fn candidates(&self, current: &str, deadline: &Deadline) -> Vec<Candidate>;
}
```

Plus the DB-derived implementations, which are the only I/O in core here:

- Open the state DB **read-only**:
  `Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)`, then
  `busy_timeout(50ms)`. There is no `OpenFlags` use in the codebase today; you
  are introducing the first one, so leave a comment saying why.
- **Never** call `Db::open()` / `Db::open_at()` on this path. Read
  `crates/thegn-core/src/db.rs:252-300` before you write this: `open()` creates
  the state dir and runs a `PRUNE_ONCE`, and `init()` sets
  `journal_mode=WAL` (a header **write**) and runs migrations under a **5 s**
  busy timeout. Any of those on a `<TAB>` press is a contract violation.
- A missing DB file, a locked DB, a schema you do not recognise ⇒ **empty
  vector**, never an error.
- Test these against a temp DB built with `Db::open_at` in a `tempdir` (the
  existing `db_tests.rs` fixtures show the pattern) — they count toward the 95%
  gate.

Config-derived sources take a `&Config` the caller supplies lazily (see step 2);
keep them pure functions over the config struct so they unit-test without I/O.

### 2. `crates/thegn-host/src/complete.rs` — the fast path

One public entry point, called from `main()`:

```rust
/// Serve a shell completion request and exit, if this process was invoked as
/// one. Costs a single env read otherwise.
pub fn maybe_complete();
```

Requirements, all from §4 of the design:

- Returns immediately unless the completion env var is set (clap_complete's
  `CompleteEnv` default is `COMPLETE`; confirm against the vendored source at
  `~/.cargo/registry/src/*/clap_complete-4.6.9/src/env/`).
- Before serving: scan the **raw argv** for `--profile <name>` /
  `--profile=<name>` and call `thegn_core::profile::reroot(...)`, so completing
  under `thegn --profile work …` reads the work profile's DB. Put the argv scan
  in `thegn-core` as a pure function and unit-test it (`--profile` last with no
  value, `=` form, `--` terminator, absent).
- Do **not** run `report_migration` / `run_startup_migration`. Consequence is
  accepted and documented in the design: an unmigrated state root yields
  structural completions only until the next real `thegn` run.
- Build the tree with `cli_help::attach(<Cli as clap::CommandFactory>::command())`
  — the same tree the parser uses — then decorate it: walk it and, for each
  `CATALOG` slot, attach an `ArgValueCompleter` to the matching arg via
  `Arg::add`. **Do not** put `add = …` on the derive in `main.rs`: keeping the
  binding in the catalog is what makes the drift test meaningful, and
  `cli_help::attach` is the existing precedent for decorating the built tree.
- **Fail open, always.** Install a silent panic hook for the duration, wrap the
  engine call in `std::panic::catch_unwind`, and on _any_ error, timeout or
  panic `std::process::exit(0)` having printed nothing. A `<TAB>` must never
  print a backtrace, a crash notice, a `config: unknown key` warning, or an
  error message.
- **Own stdout.** Nothing else may write to it on this path, for the same reason
  the stdio bridges cannot.
- **Lazy sources.** A source pays only for its own inputs, and only when the
  slot being completed asks for it. DB sources touch the DB; config-derived
  sources do a config-only load (`Config::load_layered`, **no**
  `merge_db_hosts`, **no** `clamp_to_channel`, **no** forge/git handle install,
  **no** cpucap publish) behind a `OnceCell` so two config-derived slots in one
  request load once. Suppress config warnings — check what
  `thegn_core::config::config_warn` does with its output and make sure nothing
  reaches stdout or stderr here.
- `clap_complete::{engine, env}` must be imported from **this file only**. Add a
  unit test that greps the crate's `src/` for those paths and asserts a single
  hit — the repo does this for vendor leakage already
  (`test/forge-leak-ratchet.txt`); a plain assertion test is enough here, do not
  add a new ratchet allowlist file for it.

### 3. `main.rs` — the early dispatch and `--static`

Insert the call in `fn main()` (currently line 748) in exactly this position:

```rust
mem::tune_allocator();
thegn_core::util::scrub_git_env();
crate::complete::maybe_complete();   // <-- new; may exit(0)
thegn_core::log_trace::install_panic_hook();
// … unchanged from here
```

After `scrub_git_env` because env mutation must stay single-threaded and precede
any thread; before `install_panic_hook` and before `report_migration` for the
reasons above. Leave a comment saying so — the surrounding block is already
written that way and the ordering is load-bearing.

Extend the existing variant to `Completions { shell, #[arg(long)] static_: bool }`
(clap `#[arg(long = "static")]`; `static` is a keyword). Default (`false`) emits
the dynamic registration shim via `CompleteEnv`; `--static` keeps today's exact
`clap_complete::generate` behaviour, buffered-before-write, named from `argv[0]`.
`--static` is the documented degradation path if the unstable features ever
break, so it must stay working and stay tested.

Keep the `Completions` doc comment accurate — chunk 3 owns the prose docs but
the clap `about` string lives here.

Note for later chunks (do not act on it): `completions` still dispatches through
`run_subcommand`, which loads config and opens the DB. That is now avoidable and
worth doing, but it is out of scope here — say so in the commit message.

### 4. `Cargo.toml` — features

`clap_complete = { version = "4.5", features = ["unstable-dynamic"] }` at the
workspace level; `clap = { …, features = ["derive", "env", "unstable-ext"] }`.
Add a comment at both sites naming what the unstable feature gates and pointing
at `complete.rs` as the containment boundary — a future reader upgrading clap
needs to find that in ten seconds.

### 5. Gates

**Slot-drift test** (`crates/thegn-host`, so it can see the clap tree):
`completion_slots_are_bound_or_pinned` walks
`cli_help::attach(Cli::command())` collecting every (command path, arg id) that
takes a value, and asserts each is either in `CATALOG` or pinned in
`test/completion-slot-ratchet.txt`. Shrink-only, in the shape of
`test/help-ratchet.txt`: the file carries a header comment explaining what an
entry means and that entries are paid down, never added without a reason. Seed
it with whatever the first run produces — do not chase the tail to zero in this
chunk.

**Smoke** (`test/smoke.sh`, extend the block at line 519):

- the shim is emitted for bash/zsh/fish (grep for the shell's own marker);
- `completions zsh --static` still emits `#compdef` (the existing two checks
  keep passing, adapted for the flag);
- **a `<TAB>` creates no state**: run a completion request with
  `XDG_STATE_HOME` pointed at a fresh empty dir, assert exit 0 and assert the
  dir is still empty afterwards. This is the load-bearing check of the whole
  chunk — the DB must not be created, migrated, or WAL-ified by a keypress;
- candidates appear for one DB-derived slot given a seeded temp DB;
- a coarse 300 ms ceiling on one request as a **canary** (comment it as such —
  wall-clock gates stay out of `just ci` per the repo's perf policy).

`cli_help::GROUPS` needs no change (you add no visible top-level verb) and its
drift test must stay green.

## Tests

- Core: unit tests to ≥95% lines on the new module (`just coverage`).
- Host: the drift test, the single-import-site test, the argv `--profile` scan
  test, and a test that the decorated tree still parses a normal command line
  unchanged (decoration must not alter parsing).
- Smoke as above.
- **Iterate with `just quick thegn-core` / `just quick thegn-host` and
  `cargo nextest run -p thegn-core completion`.** Run the heavy gates
  (`just test`, `just coverage`, `just lint`) **once**, at the end. The
  `PreToolUse` heavy-guard hook enforces this.
- e2e is not affected (no frame changes) — do not run or re-record it.

## Done criteria

- With the shim sourced, `thegn wt rm <TAB>` lists real worktrees;
  `thegn open <TAB>` lists real repos; `thegn attach <TAB>` lists live sessions;
  `thegn api call <TAB>` lists capability ids; `thegn config set <TAB>` lists
  config keys.
- `thegn wt <TAB>` (structure only) touches neither the DB nor the config.
- A completion request against an empty `XDG_STATE_HOME` exits 0, prints
  nothing harmful, and leaves the directory **empty**.
- No completion request ever prints a warning, an error, or a backtrace.
- `thegn completions <shell>` emits the shim; `--static` emits the `aot` script.
- Normal `thegn` launch is unchanged: one env read added before the panic hook.
- `just quick`, `just test`, `just coverage`, `just lint`, `just smoke`,
  `check-features` and `check-msrv` all green.
- `clap_complete::{engine,env}` appears in exactly one file.

## Plan B (only if step 0 fails)

Keep everything above except the clap plumbing:

- A hidden `#[command(hide = true)] Complete { index: usize, args: Vec<String> }`
  verb (`trailing_var_arg`, `allow_hyphen_values`) named `__complete`, dispatched
  from the same early position in `main()` and emitting the same
  `value\tdescription` protocol. Hidden ⇒ it must **not** be added to
  `cli_help::GROUPS` (the drift test only covers visible commands, and adding it
  would fail that test).
- Hand-written shims for bash/zsh/fish emitted by `thegn completions <shell>`,
  each ~30–40 lines, calling `thegn __complete` with the cursor index. `aot`
  output stays the answer for elvish and PowerShell.
- You now own the argument-position analysis (which slot is the cursor in), so
  keep it dumb: split on the command path, count positionals, honour `--` and
  `--flag=value`. Put that analysis in `thegn_core::completion` as a pure
  function with a thorough test table — it is the part that will be wrong.
- Everything else — catalog, candidate policy, budget, fail-open, read-only DB,
  drift test, smoke — is unchanged.

## Gotchas

- `Db::open()` is the trap. Read `db.rs:252-300` first; the WAL pragma and the
  migration are both writes.
- `install_panic_hook` prints a crash notice; that is why you install a silent
  hook around the engine call rather than relying on the global one.
- The `tg` alias: the tree is named from `argv[0]`, so the same binary serves
  both names with no special casing — but check it, because the shim's
  registration and the tree's name must agree.
- Description truncation on a multi-byte boundary panics. Test it.
- `THEGN_E2E` freeze and the perf overlay are untouched by this chunk; if you
  find yourself editing `e2e_freeze.rs` you have gone out of scope.
