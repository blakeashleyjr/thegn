# Chunk 4 — Health: make completion staleness diagnosable

**Issue:** THE-36 (right layer for shell completions).
**Design:** `.thegn/pipeline/architect/design.md` — §1 ("Why a static file is
not the whole answer either") is the motivation; §3 for the `Probe`-into-doctor
pattern this follows.

## Why

The reference guide's whole case for eval-on-startup completions is staleness:
a file written once drifts as the binary gains verbs. This design rejects that
trade — delivery moves to the packager, where a real package regenerates the
file with the binary and staleness is structurally impossible — but that only
covers packaged installs. A `cargo install`, a raw tarball, or a hand-copied
script genuinely can drift, and today nothing tells the user.

The repo's answer to "is this subsystem actually working?" is always
`thegn doctor`: every seam gets a `Probe` that reports into it, and
`test/smoke.sh` asserts the sections exist. Completions should be no different.
Detection beats ceremony: instead of asking every user to re-source a script at
every shell launch forever, tell the few who need it, once, exactly what to run.

This chunk is independent of the other three. It reads the filesystem and calls
the completion generator that has existed since the CLI-namespaces change, so
it works today and keeps working whichever way chunks 1 and 2 land.

## Files you own

- `crates/thegn-host/src/completions_health.rs` (new)
- `crates/thegn-host/src/cmd/doctor.rs` (a small call-in + a report section)
- `crates/thegn-host/src/main.rs` — **one line only**, the `mod` declaration.
  Add nothing else there; another chunk is editing that file.
- `test/smoke.sh` — **only** the `doctor` block. Another chunk owns the
  completions block around line 519; do not touch it.

Do not touch `crates/thegn-core/`, `nix/`, `docs/`, `openspec/`, or
`.github/`.

## Approach

### 1. `completions_health.rs`

Two pure functions plus one thin I/O wrapper, so the logic is testable without a
filesystem.

**Search paths (pure).**
`fn search_paths(env: &Env, exe: &Path) -> Vec<Target>` where `Target { shell,
dir, file_name, command }`. Take the environment as an injected struct (the
codebase has this pattern — `thegn_core::config::ProcessEnv` and
`termcaps::TermEnv::from_env()` are the models) so the tests do not mutate
process env.

Per shell, in priority order, for **both** the `thegn` and `tg` command names:

- **zsh** → file `_<cmd>` in: `$XDG_DATA_HOME/zsh/site-functions`,
  `~/.zsh/completions`, `<prefix>/share/zsh/site-functions`,
  `/usr/local/share/zsh/site-functions`, `/usr/share/zsh/site-functions`
- **bash** → file `<cmd>` in: `$XDG_DATA_HOME/bash-completion/completions`,
  `~/.local/share/bash-completion/completions`,
  `<prefix>/share/bash-completion/completions`, `/etc/bash_completion.d`,
  `/usr/share/bash-completion/completions`
- **fish** → file `<cmd>.fish` in: `$XDG_CONFIG_HOME/fish/completions`,
  `~/.config/fish/completions`, `<prefix>/share/fish/vendor_completions.d`,
  `/usr/share/fish/vendor_completions.d`

`<prefix>` is derived from `std::env::current_exe()` by walking up out of
`bin/` — that is what finds a Nix-store or `~/.local` install, and it is the
case that matters most. Resolve symlinks when deriving it (`current_exe` on a
`tg` invocation lands on the alias).

**Classification (pure).**
`fn classify(installed: &[u8], generated: &[u8]) -> State` returning
`Fresh | Stale | Dynamic`:

- if `installed` contains the dynamic-shim marker (the completion env var name,
  `COMPLETE`, appearing in the script body) ⇒ `Dynamic` — a shim asks the binary
  at completion time and therefore **cannot** go stale. Report it as such rather
  than diffing it.
- else byte-equal ⇒ `Fresh`; differing ⇒ `Stale`.

This marker check is what keeps this chunk decoupled from chunk 2: it is correct
whether the installed file is today's `aot` script or tomorrow's shim, and it
needs no shared type.

Normalise trailing whitespace/newline before comparing — a packager or an editor
may have added one, and reporting `stale` for a trailing `\n` is noise.

**The wrapper.** `fn report() -> Report` walks the targets, reads the first
existing file per (shell, command), generates the current script in-process with
`clap_complete::aot::generate` over
`cli_help::attach(<Cli as clap::CommandFactory>::command())` with the matching
command name, and classifies. Absent everywhere ⇒ one `Absent` entry for that
(shell, command). Every read is best-effort: a permission error is `Absent`,
never a doctor failure.

Generation happens **only** in `doctor` — this is not on any hot path, so the
cost is irrelevant, but do not be tempted to cache it to disk.

### 2. `cmd/doctor.rs`

Add a **Completions** section to the text report, in the existing section style:

```
Completions
  zsh    thegn  fresh    /nix/store/…/share/zsh/site-functions/_thegn
  zsh    tg     fresh    /nix/store/…/share/zsh/site-functions/_tg
  bash   thegn  stale    ~/.local/share/bash-completion/completions/thegn
  fish   thegn  absent   — run: thegn completions fish > ~/.config/fish/completions/thegn.fish
```

Rules:

- a `stale` or `absent` row prints the exact command that fixes it, with the
  destination path filled in — the whole point of the section is that the user
  does not have to go read `docs/cli.md`;
- `dynamic` rows say so and explicitly note that they never go stale;
- everything `fresh`/`dynamic` collapses to a one-line summary, in the spirit of
  the catalog-coverage summary doctor already prints — do not make a healthy
  install scroll.

Mirror it into `thegn doctor --json` under a `completions` key: an array of
`{shell, command, state, path}`. Keep the JSON shape stable and emit it through
whatever helper the rest of doctor's JSON already uses.

Keep the edit to `doctor.rs` small — call into your module, do not inline the
logic. (`CLAUDE.md`: keep the god-files from growing.)

### 3. `test/smoke.sh`

In the **doctor** block only:

- `thegn doctor` prints a `Completions` section;
- `thegn doctor --json` contains the `completions` key and parses as JSON;
- doctor exits 0 when no completions are installed anywhere (the common case on
  a CI runner) — an absent install is a report line, never a failure.

## Tests

- Unit tests in `completions_health.rs`:
  - `search_paths` honours `XDG_DATA_HOME`/`XDG_CONFIG_HOME` when set and falls
    back correctly when unset; derives `<prefix>` from a `…/bin/thegn` exe path;
    covers both command names;
  - `classify` — identical bytes ⇒ `Fresh`; a one-verb difference ⇒ `Stale`; a
    body containing the shim marker ⇒ `Dynamic` even when it differs; trailing
    newline differences ⇒ `Fresh`;
  - a `tempdir` round-trip: write a generated script to a fake target dir, point
    the injected env at it, assert `Fresh`; mutate a byte, assert `Stale`.
- `cargo nextest run -p thegn-host completions_health` while iterating; run the
  heavy gates once at the end (`CLAUDE.md` dev-loop policy — a `PreToolUse` hook
  enforces it).
- `just smoke` for the doctor checks.
- e2e is not affected (`doctor` is a CLI verb, not a frame) — do not run or
  re-record it.

## Done criteria

- `thegn doctor` reports, per shell and per command name (`thegn` and `tg`),
  where completions are installed and whether they are current.
- A stale or absent entry prints the exact fix command with the real path.
- A dynamic shim is reported as `dynamic`, not diffed and not called stale.
- `thegn doctor --json` carries the same data under `completions`.
- `thegn doctor` still exits 0 with nothing installed.
- `just lint`, `cargo nextest run -p thegn-host`, `just smoke` green.
- `main.rs` gained exactly one line.

## Gotchas

- **Do not compare mtimes.** Nix normalises store timestamps to 1970-01-01, so
  "installed file older than binary" reports every correct Nix install as stale.
  Content comparison is the only thing that works across install paths.
- `current_exe()` through the `tg` symlink resolves to the real binary; derive
  `<prefix>` from the resolved path, but keep reporting both command names.
- `clap_complete::generate` panics on a broken pipe — the existing `Completions`
  handler buffers before writing for exactly this reason. You are generating
  into a `Vec<u8>` anyway; keep it that way.
- Doctor runs through `run_subcommand`, so config and the DB are already loaded
  by the time you are called — you have no fast-path constraints here, unlike
  chunk 2. Do not import its constraints by mistake.
- Another chunk is editing `main.rs` and the completions block of
  `test/smoke.sh`. Stay inside your lines so the merge is clean.
