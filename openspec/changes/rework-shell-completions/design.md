# Design — rework-shell-completions

## Context

THE-36 asks which layer owns shell completions. The reference guide that seeded
it frames the choice as **dynamic** (`eval "$(tool completion zsh)"` in the rc:
regenerated at every shell launch, never stale, costs a process spawn per shell)
versus **static** (a file written once: zero startup cost, goes stale after an
update). That framing collapses two independent axes — _where the script comes
from_ and _what a `<TAB>` can know_ — and its recommended default is the one
option that is actively wrong for thegn.

## Decision: three layers, each owning exactly one thing

| Layer                           | Owns                                                     | Mechanism                                                                                                                          |
| ------------------------------- | -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **Packaging** (build time)      | _Delivery_ — getting a file on disk                      | `nix/package.nix` installs into the XDG completion dirs; the release ships one arch-independent asset. The user's rc does nothing. |
| **The binary** (`<TAB>` time)   | _Answers_ — structure **and** values                     | The installed file is a registration **shim**; its body calls back into `thegn` on each `<TAB>`.                                   |
| **`thegn-core`** (compile time) | _Policy_ — which slot takes which values, and the budget | A pure slot→source catalog + candidate policy, drift-guarded and coverage-gated.                                                   |

**The user's shell rc is not a layer.** `thegn completions <shell>` survives as
the escape hatch for installs no packager touched (`cargo install`, a raw
tarball), and the docs stop recommending `eval "$(…)"`.

### Why eval-on-startup is wrong here specifically

thegn spawns a shell **per pane**, and a warm reattach restores every pane in a
session at once. An rc-file `eval` therefore adds a full `thegn` process launch
— today that path runs through `run_subcommand`, which loads the layered config
and **opens the DB** before it reaches the generator — to **every pane thegn
itself opens**, against a stated sub-300 ms launch and 0%-idle invariant. `gh`
and `kubectl` do not have this problem because they are not the thing spawning
your shells.

A statically installed file has none of that cost: zsh autoloads `_thegn` from
`fpath` lazily on the first `<TAB>`, and bash-completion's dynamic loader does
the same. **So: install a file, always.**

### Why a static file is not the whole answer either

Staleness is real but is a _packaging_ problem, not a shell problem:

- **Any real package** regenerates the completion file in the same derivation
  that built the binary. It cannot be stale. Solved at the packaging layer, with
  no user discipline.
- **`cargo install` / raw tarball** genuinely can drift. Handled by detection,
  not ceremony: `thegn doctor` compares the installed file against what the
  running binary emits and reports `fresh`/`stale`/`absent` with the one command
  that fixes it.

And what a static structural script can _never_ do is complete a **value**:
`thegn wt rm <TAB>` cannot know your worktrees, `thegn open <TAB>` your repos,
`thegn attach <TAB>` the daemon's live sessions. For a noun-verb CLI over a live
state DB, that is where all the actual value of completion sits.

### The synthesis

Install, at package time, a **static registration shim whose body invokes the
binary at `<TAB>` time**. Startup cost zero; staleness structurally impossible
(the shim contains no command names); values live. One mechanism, not two. This
is exactly what `clap_complete::env::CompleteEnv` emits — the only difference
from the guide's dynamic mode is that the registration script is captured **to a
file at package time** instead of `eval`ed at every shell startup. That single
change is the whole of the "right layer" answer.

## The `<TAB>`-time fast path — contract

Every `<TAB>` press is a process launch with a human waiting on it, so this is
specified as a contract rather than as advice. The `run_subcommand` path is
disqualified: before dispatching it resolves the channel, calls
`Config::load_layered`, calls `merge_db_hosts` (**opens the DB read-write**),
installs the log subscriber, emits preset/pipeline warnings, installs the forge
and git handles, and publishes the cgroup limit policy.

**MUST:**

- **Dispatch from the top of `main()`** — after allocator tuning and git-env
  scrubbing (env mutation must stay single-threaded and precede any thread) and
  before the panic hook and migration report; and after a **raw-argv scan** for
  `--profile` plus the profile reroot, so completing under
  `thegn --profile work …` reads the work profile's DB rather than offering
  another profile's worktrees.
- **Cost nothing when not completing** — one `env::var_os` read on the normal
  launch path.
- **Fail open.** Any error, timeout, missing DB, unmigrated state root or panic
  ⇒ exit 0 having printed nothing; the shell falls back to filename completion,
  which is today's behaviour. Never a backtrace, a crash notice, a
  `config: unknown key` warning, or an error message.
- **Never create state.** The DB is opened read-only with a short busy timeout —
  not the normal open path, which sets WAL mode (a header write), runs
  migrations and takes a 5 s timeout. `<TAB>` on a machine that has never run
  thegn leaves the filesystem untouched, asserted in smoke.
- **Own stdout.** One candidate per line in the shell's protocol; nothing else
  may write there, for the same reason the stdio bridges cannot.
- **Skip the startup migration**, like the stdio bridges. Consequence, accepted
  and documented: an unmigrated state root gets structural completions only
  until the next real `thegn` run.

**Sources are lazy and independent.** A source pays only for its own inputs, and
only when the slot being completed asks for it. DB-derived sources touch the DB;
config-derived sources do a config-only load with warnings suppressed — no DB,
no clamp, no handle installs. Completing `thegn wt <TAB>` (structure only)
touches neither.

**Budget.** `THEGN_COMPLETE_BUDGET_MS` (default 100) is a deadline checked
between sources; on expiry, emit what has been gathered and stop. No watchdog
thread — the budget is enforced by bounded I/O (read-only DB with a short busy
timeout, no network, no subprocess), and the deadline is the belt to those
braces. Smoke asserts a coarse 300 ms ceiling as a **canary**, not a perf gate;
wall-clock gates stay out of `just ci` per the repo's perf policy.

### One accepted exception, recorded

`--profile <name>` **does** create one directory: the reroot `mkdir -p`s that
profile's state dir. Without it a `<TAB>` would read the _shared_ DB and offer
another profile's worktrees, which is worse than an empty directory. The default
profile — every completion that does not name one — creates nothing, and that is
what smoke asserts.

## Mechanism, risk, and the specified fallback

**Plan A (taken).** `clap_complete::env::CompleteEnv` for the shell protocol and
the registration shim, `clap_complete::engine` + `ArgValueCompleter` for
candidates, attached by walking the built tree from the catalog — the same
"decorate the built `Command`" pattern `cli_help::attach` already uses, which
keeps the derive clean and the catalog the only place a slot is bound.

**The risk, stated plainly:** this needs two explicitly-unstable Cargo features —
`clap_complete/unstable-dynamic` (gates `engine` and `env`) and
`clap/unstable-ext` (gates `Arg::add`, which is how a completer attaches). Both
can break on a minor bump. Containment:

- both are additive;
- the imports are confined to `crates/thegn-host/src/complete.rs`, asserted by a
  unit test in the forge-leak-ratchet spirit;
- `Cargo.lock` is committed, so an upgrade is deliberate and a break is a
  compile error the gates catch;
- the stable `aot` path keeps shipping as `thegn completions <shell> --static`,
  so a breakage degrades to today's behaviour rather than to nothing.

**Plan B (specified, not preferred).** If either feature stops building or
misbehaves: a hidden `thegn __complete` verb taking the raw words plus a cursor
index, emitting the same `value\tdescription` protocol, with three hand-written
shims (bash/zsh/fish) and `aot` for elvish/PowerShell. Same core policy module,
same fast-path contract, same delivery layer — only the shell-protocol plumbing
changes, and it becomes ours to maintain.

## The slot catalog

`CATALOG` maps `(command path, arg id) → SourceKind` and is the single source of
truth for which slot gets which values. The drift test walks the live clap tree
against it, so a new verb with an uncompletable argument is a test failure with a
one-line ratchet escape — the same deal `cli_help::GROUPS` and the help ratchet
already offer.

- **Implemented** (DB-derived): worktree, repo/workspace, session, host.
  (Config-derived): env, profile, theme, agent, tool, plugin, mcp-server,
  config-key. (Catalog-derived): capability. (Keymap-derived): action.
- **`reserved`, with the reason recorded in the enum**: `branch` — git I/O the
  fast path declines to pay for; `pr` and `issue` — network, which a `<TAB>` may
  never touch. Revisit `branch` once the git seam can be built without a full
  config load.
- **Structural** — subcommand names, flags, and `ValueEnum` arguments such as
  `completions <shell>` — is a _declared_ decision in the catalog, not an
  unclassified slot.

Two candidate-policy calls worth recording:

- **Shell-hostile values are dropped, not escaped.** Every shell protocol here
  is line-oriented with an in-line separator, so a value carrying a newline or
  tab does not merely render badly — it desynchronises the parse, turning one
  candidate into two. No escaping works across all five shells.
- **Descriptions are sanitised, not dropped** (control chars collapsed,
  truncated), because losing a description must never lose the value it
  describes.

## Rejected alternatives

- **`eval "$(thegn completions zsh)"` in the user's rc** — the industry default,
  and the reference guide's recommendation. Rejected for the reason above: thegn
  spawns a shell per pane, so this puts a config-loading, DB-opening process
  launch into every pane restore. It is also strictly worse than the shim on its
  own terms: the shim is _also_ never stale, and costs nothing at startup.
- **Static `aot` scripts only** (what `package-shell-completions` proposed) —
  correct on delivery, but it can never complete a value, and it re-introduces
  staleness for every install path a packager does not own. Kept as `--static`,
  the degradation path.
- **A third-party completion framework (carapace or similar)** — a second vendor
  to track and a second definition of the CLI to keep in sync, against the
  one-source rule. The binary already knows its own tree.
- **Shipping completions inside every per-target release archive** — the CLI
  tree has no `#[cfg]`-gated `Command` variants, so all targets emit identical
  scripts; per-target copies are duplication, and the musl/darwin legs cannot
  always execute the binary they just built. One arch-independent asset instead.
- **mtime-based staleness detection** — Nix normalises store timestamps to
  1970-01-01, so "installed file older than binary" reports every correct Nix
  install as stale. Content comparison is the only thing that works across
  install paths.
- **A watchdog thread for the budget** — a thread per `<TAB>` to bound work that
  is already bounded by construction. The deadline between sources is enough.

## Invariants this change touches

- **Render damage channel: none.** No frame, no wake path, no event-loop work.
  The `<TAB>` path exits before the compositor exists; `thegn doctor` is a CLI
  verb. e2e is not re-recorded.
- **SQLite: no schema change, no `user_version` bump.** The completion path
  opens the existing DB **read-only**.
- **Help context: none.** No new interactive surface, action, keybind, zone or
  panel section, so no `zone:*` / `panel:*` mapping is needed; `docs/help/cli.md`
  already claims this surface and gains prose only.
- **`thegn-core` stays substrate-free** — the new `completion` module adds no
  dependency, and is **not** added to the justfile's `cov_ignore`, so it is
  gated at 95% lines.
- **Seams, not vendors** — value sources are an object-safe trait with
  implemented-or-`reserved` kinds and a `doctor` projection.
- **God-files** — `main.rs` gains ~5 lines; everything host-side lives in new
  sibling modules (`complete.rs`, `completions_health.rs`).
