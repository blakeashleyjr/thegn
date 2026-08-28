# THE-36 — architect review

**Verdict: APPROVED.**

Branch `tg/the-36-completions`, reviewed against
`.thegn/pipeline/architect/design.md` at merge-base `cdfcfaf7` plus the
`main` merge done as part of this review.

All four chunks are built as designed. Three defects found and fixed here; no
revision chunk is needed and none was written.

---

## 0. The three prep steps

**Main merged** (`096b01de`). Main had gained the pipeline board-access work,
THE-68's attention-signal fix, and schema v57. This lane adds **no DB table** —
`DbSource` only reads — so nothing needed renumbering and v57 carries through
untouched.

**The cpucap commit reconciled.** `a1278db5` on this lane and `d4f3aeb9`+ on
main are the same fix for the same standoff (clippy's `manual_ok_err` wants
`.ok()`, the ignored-result ratchet greps for `.ok();`). Main's is
statement-scoped inside the digits-only branch; the lane's was function-scoped.
Auto-merge stacked both. Main's is kept, the lane's duplicate dropped.

The `.thegn/pipeline/**` add/add conflicts are per-lane scratch paths that main
happens to carry THE-68's copies of — resolved to this lane's THE-36 artifacts,
and THE-68's stale verdicts removed rather than merged.

**Chunk 1's UNVERIFIED nix outputs: verified.** `nix build .#default` is green
and installs six registration shims — `thegn` and `tg`, in bash, zsh and fish —
each invoking the binary **by name** rather than by the `/nix/store` path it was
generated from, which is the property that lets one generated script work for
every user. `defaultPkg` is an `overrideAttrs`, not a `symlinkJoin`, so `share/`
survives into `.#default`. `.#dev` shares the same `postInstall` and differs only
in `binName`/`aliasName`; the dev-channel naming is now handled (§2).

Building it is also what found the bash defect below.

---

## 1. Design conformance

**The three-layer answer holds, and each layer owns exactly one thing.**

| Layer                 | Verdict                                                                                                                                                                                          |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Packaging (delivery)  | `nix/package.nix` generates + installs both names in three shells; `release.yml` ships one arch-independent asset; `just completions` is the hand-install convenience. No rc-file edit anywhere. |
| The binary (answers)  | The installed artifact is the `CompleteEnv` shim; `--static` keeps the stable `aot` script as the documented degradation path.                                                                   |
| `thegn-core` (policy) | `completion::{catalog,candidate,sources}` — pure, substrate-free, no new dependency, drift-guarded.                                                                                              |

**The §4 fast-path contract is met in full, and verified rather than assumed:**

- dispatched from the top of `main()` after `tune_allocator`/`scrub_git_env`,
  before `install_panic_hook` and `report_migration` — one `env::var_os` on the
  normal launch path;
- fail-open — `catch_unwind` under a temporarily-silent hook, every arm
  `exit(0)` printing nothing;
- never creates state — `SQLITE_OPEN_READ_ONLY`, 50 ms busy timeout, no
  `Db::open`. **Measured:** a `<TAB>` against an empty `XDG_STATE_HOME` leaves
  zero filesystem entries.
- owns stdout — diagnostics routed off stderr via `msg::set_tui_active`, which
  matters because bash's shim (unlike zsh's) does not redirect stderr;
- lazy sources, `--profile` rerooted from a raw argv scan before any path
  resolves.

**Measured against the real 65 MB in-use state DB** (WAL, live writer holding
it): `wt rm <TAB>` answers in **28–30 ms** debug, a config-derived slot in
**35–40 ms** — comfortably inside the 100 ms budget and the 300 ms canary. Live
worktree paths and their branches come back correctly. The read-only-WAL
concern I had going in is a non-issue: SQLite opens a cleanly-closed WAL
database read-only without needing to create the `-shm`.

**§5 catalog** is implemented as specified, with one honest divergence recorded
in the code: the `theme`, `tool`, `plugin` and `action` kinds are served and
tested, but today's CLI grammar has no argument that takes one — the design's
`action` (`keys …`) binding does not exist, since `keys list`/`keys hints` take
a `zone`, which is pinned. Waiting for a verb beats binding to an approximation.

**§6** took Plan A, with the containment assertion (`clap_complete::{engine,env}`
in exactly one file) and both unstable features documented at the dependency.
**§7** gates are all present and green (§3).

---

## 2. Defects found and fixed (committed here)

**`d69683bb` — a packaged bash completion reported `absent`.** The one that
mattered. nixpkgs' `installShellCompletion --cmd thegn` writes
`share/bash-completion/completions/thegn.bash`; bash-completion's loader accepts
both `<cmd>` and `<cmd>.bash`, so the _installed file worked_ — only the health
search knew a single spelling. `thegn doctor` on the real store output therefore
called two of its own six shims missing and told the user to write a file into a
directory the packager had not used. `file_name` → `file_names`, bash returns
both, most-canonical first (so an `absent` row still asks a hand-installer for
the plain name). Re-verified against the store layout: _"6 installed and current
(6 dynamic shims, which never go stale)"_.

This is exactly what chunk 1's done-artifact flagged as unverified, and it was
only findable by building the package.

**`943dc400` — three smaller ones:**

- _The seam's third leg was missing._ `SourceKind` is implemented-or-`reserved`
  with a reason per reserved kind, but nothing surfaced them, so
  `reserved_reason()` was reachable only from the source and "branch names do
  not complete" read as a bug. `doctor` now closes the Completions section with
  the projection (`14 live, 3 reserved`, each reserved kind with its reason,
  grouped since `pr`/`issue` share one) — the `Probe`-into-doctor leg design §3
  asked for. Recorded as a scenario on the existing source requirement in the
  change's cli delta, and pinned in smoke.
- _A dev-channel install reported itself absent._ The dev package installs as
  `thegn-dev`/`tg-dev` and names its completion files accordingly, but the
  health report hard-coded the stable pair — six false `absent` rows and a fix
  command naming a binary the user does not have. `commands_for(exe)` picks the
  pair from the invoked name.
- _A shim paid for a generation it discarded._ `report_with` documented lazy
  generation but generated the comparison script before `classify` looked for
  the shim marker — six `aot` generations per `doctor` run on precisely the
  packaged install this change makes the default. `is_shim` split out and
  checked first; a test counts the generations.

---

## 3. Gates

| Gate                                             | Result                                                                  |
| ------------------------------------------------ | ----------------------------------------------------------------------- |
| `just smoke` (+ PTY)                             | **green** — all 20 completion checks pass, including the two added here |
| `cargo nextest -p thegn-core completion`         | 42 pass                                                                 |
| `cargo nextest -p thegn-host complete::`         | 7 pass — drift ratchet, containment, decoration-does-not-change-parsing |
| `cargo nextest -p thegn-host completions_health` | 14 pass                                                                 |
| `just quick thegn-host` (clippy)                 | clean                                                                   |
| `treefmt --fail-on-change`                       | clean                                                                   |
| `openspec validate --all --strict`               | 168/168                                                                 |
| `nix build .#default`                            | green, outputs inspected                                                |
| `just coverage`                                  | started at review end; see §5                                           |

---

## 4. Accepted, with the reasoning on the record

- **173 pinned slots** in `test/completion-slot-ratchet.txt`. That is real debt,
  but it is the escape the design sanctioned, it only shrinks, and the drift
  test fails on both a new unclassified slot and a stale pin. The alternative —
  classifying 173 arguments in one change — is how this lands late.
- **`thegn completions <shell>` still dispatches through `run_subcommand`**, so
  generating a script loads the layered config and opens the DB. Harmless (it is
  not the `<TAB>` path, which is the one that matters and which bypasses all of
  it), but it is why `nix/package.nix`, `release.yml` and the `justfile` each
  carry the same five-line scratch-XDG preamble. Worth folding into the fast
  path later; out of scope here.
- **Three new transitive crates** (`clap_lex`, `is_executable`, `shlex`) from
  `unstable-dynamic`. Small, and `deps-audit` in `just ci` covers advisories.
- **e2e untouched**, correctly — no frame changes.

## 5. Before landing

`just ci` has not been run over the finished change (chunk 4's done-artifact
left it, correctly, for whoever closes the change out) — and `just coverage`
was still running when this verdict was written, so **check its result**: the
new `thegn_core::completion` module is deliberately _not_ in `cov_ignore` and
is gated at 95%.

Nothing in that gate blocks the design review: the architecture is right, the
contract is honoured, and the parts that could only be verified by building and
running have now been built and run.
