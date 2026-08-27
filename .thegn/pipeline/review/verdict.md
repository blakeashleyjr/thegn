# THE-36 — security / test / bug review

**Verdict: PASS.** Ready for the merge queue.

Branch `tg/the-36-completions`, reviewed at `8ec8c469` (architect verdict
APPROVED, main merged in-review at `096b01de`). One defect found, fixed and
committed here (`2f499473`); nothing else blocks.

Adversarial focus, as briefed: the `<TAB>` engine, packaged install paths, the
doctor negative path, and the unstable clap pins — plus the standard sweep.

---

## 1. The defect (fixed here — `2f499473`)

**A non-UTF-8 word on the command line made a `<TAB>` print a Rust panic and
exit 101.** Reproduced against the pre-fix binary:

```
$ COMPLETE=bash _CLAP_COMPLETE_INDEX=3 thegn -- thegn wt rm $'/srv/caf\xe9/'
thread 'main' panicked at library/std/src/env.rs:876:51:
called `Result::unwrap()` on an `Err` value: "\xE9"
exit 101
```

`maybe_complete` read argv with `std::env::args()`, which panics on an argument
that is not valid UTF-8 — and the arguments on this path _are_ the words off the
user's command line, passed verbatim by the shim. It did so **before** the
silent panic hook was installed (`complete.rs:103`) and **outside** the
`catch_unwind` boundary, so every clause of the module's own contract inverted at
once: it printed a backtrace, it printed on stderr, and it exited non-zero.

It is visible in practice because **bash's and fish's generated shims do not
redirect stderr** — only zsh's carries `2>/dev/null`. I re-read all three
generated scripts to confirm that, so this lands on the prompt mid-keystroke. One
latin-1 byte in a path being completed is the whole trigger.

This is the same wall `0f9b5c87` built ("a `<TAB>` can never fall through to a
compositor launch"). That fix holds — see §2 — this was the remaining hole in it.

The fix keeps the existing shape: the silent hook moves above the first thing
that can panic, the reroot moves inside `catch_unwind` (it was never exempt from
"prints nothing, exits 0"), and `lossy_argv()` (`args_os` + `to_string_lossy`)
replaces `args()`. Lossy is correct rather than convenient here — it feeds only
the `--profile` scan, whose value `profile::reroot` slugifies anyway, and the
word being completed reaches `wire` as an `OsStr` and is already dropped there
when it is not UTF-8. Verified rc=0 / empty stderr in bash, zsh and fish, and for
a non-UTF-8 `--profile` value.

Pinned in smoke on the **failure** path, which is what was missing:
`a non-UTF-8 word completes quietly instead of panicking` asserts exit 0 _and_
empty stderr. The existing stderr check only ever passed ASCII.

---

## 2. `<TAB>` can never launch a compositor — tried to break it, could not

Ran every fall-through I could construct against the real binary, isolated HOME
and XDG. All exit 0, none reaches `run_subcommand`:

| Input                                                  | Result                                      |
| ------------------------------------------------------ | ------------------------------------------- |
| `COMPLETE=notashell` (no adapter)                      | exit 0, silent — the `0f9b5c87` case        |
| `COMPLETE=1`                                           | exit 0, silent                              |
| `COMPLETE=0` / `COMPLETE=` (clap's disabled spellings) | falls through _deliberately_, normal launch |
| `_CLAP_COMPLETE_INDEX` = `99999` / `-1` / `abc`        | exit 0, silent                              |
| no `--` separator (registration request)               | exit 0, emits the shim                      |
| `THEGN_COMPLETE_BUDGET_MS=notanumber`                  | exit 0, defaults to 100 ms                  |
| non-UTF-8 word                                         | **exit 101 + backtrace** → fixed (§1)       |

The decide-then-always-exit restructure is what makes this hold: `maybe_complete`
commits to exiting before anything can return an `Err`, so served / refused /
panicked share one `exit(0)`.

**No error is swallowed that should not be.** Every `.ok()` / empty-vec on this
path is the fail-open contract, stated at the definition. The one place that
_would_ escape the guard is `msg::die`, which writes to stderr and exits 1
regardless of `set_tui_active` — I checked, and nothing on the completion path
(config load, profile reroot) calls it. `Config::load_layered` has its own
`catch_unwind` in `complete.rs::config()`, and spawns no subprocess.

**Hostile argv/env.** `--profile ../../../../tmp/pwn` at `<TAB>` time does not
traverse: `reroot` → `normalize_name` → `util::slugify` flattens it to
`profiles/tmp-pwn-the36/`. Verified nothing was created outside the state root.
Candidate values are sanitised before they reach the wire — control characters
drop the value rather than escaping it (the right call: no escaping is common to
all five shell protocols, and a newline desynchronises the parse rather than
rendering badly), descriptions are flattened and char-boundary truncated.

**Latency.** The smoke canary (300 ms, ~6× observed debug cost) is the right
instrument, correctly kept out of `just ci` per the perf policy. The `Deadline`
is checked between sources, and the I/O is bounded by construction — read-only
SQLite with a 50 ms busy timeout, one config read, no subprocess, no network, no
git. `thegn wt <TAB>` touches neither DB nor config.

**Never creates state.** Smoke asserts a `<TAB>` against an empty `XDG_STATE_HOME`
leaves zero filesystem entries, and `SQLITE_OPEN_READ_ONLY` without `CREATE` is
what makes that structural rather than incidental. One accepted exception,
documented at the contract: `--profile <name>` mkdir's that profile's tree
(`root`, `state`, `config`, `gnupg`), so a typo'd profile name at `<TAB>` time
leaves a junk profile directory. It is the deliberate trade — the alternative is
reading another profile's worktrees — and a typo on a real run does the same.
Worth a line in the design's accepted-costs list; not a blocker.

## 3. Packaging — no path or privilege surprises

- **`nix/package.nix`**: writes only under `$out` and `$TMPDIR`; `HOME`/XDG
  redirected to scratch before running the just-built binary; correctly ordered
  after the alias symlink and before `wrapProgram`; guarded by
  `buildPlatform.canExecute hostPlatform` so cross builds still produce a
  package. Redirecting to files rather than `<(…)` is right — process
  substitution would hide a generator failure and install a truncated script.
  nixpkgs' builder is `set -eu -o pipefail`, so a failed generation aborts.
- **`release.yml`**: `TAG` reaches the shell as an **env var**, never string
  interpolation, in both the generate and upload steps — no expression-injection
  seam. `set -euo pipefail`; the archive is staged in `mktemp -d` and rooted with
  `-C "$stage" .`. Only nit: a tag containing `/` would produce an unwritable
  asset filename, which fails loudly at generate time rather than doing anything
  surprising.

## 4. Doctor negative path — `37950f4b`'s claim verified

`Report::needs_attention()` is **rendering only**; nothing derives an exit code
or a health verdict from it, so six `absent` rows print six fix commands and
`doctor` still exits 0. Confirmed in smoke (`doctor exits 0 with no completions
installed, and names the fix`) and by reading `doctor::run` — the JSON path
returns early, so `--json` stays parseable, which the python assertion pins.

The two doctor fixes from the architect pass hold up: `commands_for(exe)` picks
the dev pair off the invoked name, and bash returning both `<cmd>` and
`<cmd>.bash` (most-canonical first) matches what `installShellCompletion`
actually writes. `SHIM_MARKER = "COMPLETE="` is a thin marker, but it cannot
false-positive on a real `--static` script:
`the_real_generator_answers_for_every_shell` asserts `classify(script, script) ==
Fresh` for all six real generator outputs, which would fail the moment an `aot`
script contained the marker.

## 5. Unstable clap pins — gate discipline is intact

`clap/unstable-ext` and `clap_complete/unstable-dynamic` are both documented at
the dependency with the breakage risk and the fallback. Containment is asserted,
not just intended: `clap_complete_is_imported_once` walks `crates/thegn-host/src`
and fails unless `clap_complete::{engine,env}` appears in exactly `complete.rs`.
The degradation path (`--static` → the stable `aot` generator) is smoke-tested
for bash and zsh, so it cannot rot while unused. That is the right shape for an
unstable pin.

## 6. Tests and ratchets

- Failure paths are covered where it counts: missing DB, junk-not-a-database,
  schema without the table, expired deadline (per source _and_ per kind at the
  host boundary), non-UTF-8 partial word, unparseable budget, control-character
  values, multi-byte truncation, a stale catalog row, a new unclassified slot.
- Ratchets clean: `ignored-result` (323 pinned, no new entries — the fix adds no
  `let _ =`), `json-emit`, `forge-leak`, and the lane's own
  `completion_slots_are_bound_or_pinned` (both directions, plus stale-pin
  detection).
- Coverage risk from my fix: none. `lossy_argv` is in `thegn-host`, which is not
  coverage-gated; `thegn_core::completion` is untouched by it.

## 7. Gates run here

| Gate                                                                                                 | Result                                             |
| ---------------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| `cargo nextest -p thegn-core -p thegn-host` (completion / complete:: / completions_health / profile) | **118 pass**                                       |
| `just smoke` + PTY                                                                                   | **green** — 21 completion checks incl. the new one |
| `just quick thegn-host` (clippy)                                                                     | clean                                              |
| `treefmt --fail-on-change`, `shellcheck test/smoke.sh`                                               | clean                                              |
| `test/ratchet.sh` ignored-result / json-emit / forge-leak                                            | clean                                              |
| Manual: 11 adversarial `<TAB>` invocations, 3 generated shims read, profile-traversal probe          | see §2                                             |

Not re-run (the architect pass covers them, §3 of that verdict):
`nix build .#default`, `openspec validate --all --strict`, the full nextest
sweep. `just ci` / `just coverage` remain the pre-land gate — the architect
verdict's §5 note still stands, and `thegn_core::completion` is deliberately
outside `cov_ignore` at 95%.

---

Merge step is `thegn integrate` — not run here.
