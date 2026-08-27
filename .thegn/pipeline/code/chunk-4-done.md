# Chunk 4 — done: health (completion staleness is diagnosable)

Issue THE-36. Branch `tg/the-36-completions`. **Final code chunk.**

## Commits (3, on the branch)

| commit     | what                                                                                |
| ---------- | ----------------------------------------------------------------------------------- |
| `84528adf` | `completions_health.rs` + the doctor text section and `--json` key + the `mod` line |
| `37950f4b` | three smoke checks in the doctor block                                              |
| `6876fc82` | the architect design doc tripped `test/brand-guard.sh` (see "One thing outside…")   |

Plus `openspec/changes/rework-shell-completions/tasks.md`: 4.1–4.5 and 5.2
ticked, matching how chunks 1–3 recorded themselves.

## What landed

`crates/thegn-host/src/completions_health.rs` (new, ~460 lines with tests) —
two pure functions plus one thin I/O wrapper:

- **`search_paths(env, exe) -> Vec<Target>`** — per shell (zsh/bash/fish) and
  per command name (`thegn` **and** `tg`, since a script registered for one
  never fires for the other), in priority order: user locations → this
  install's prefix → system. `Env { home, xdg_data_home, xdg_config_home }` is
  injected (the `thegn_core::config::ProcessEnv` pattern), so no test mutates
  the process environment. `<prefix>` comes from walking up out of `bin/` — the
  Nix-store / `~/.local` case, which is the one that matters most, since that
  is where chunk 1's packager put the files. `report()` canonicalises
  `current_exe()` first, so a `tg` invocation resolves to the real binary.
- **`classify(installed, generated) -> State`** — the shim marker (`COMPLETE=`,
  the env-var assignment every `clap_complete` registration carries) short-
  circuits to `Dynamic` and is **never diffed**; otherwise byte-equality after
  `trim_ascii_end` ⇒ `Fresh` / `Stale`. The marker is a local constant with a
  comment saying why it is not a reach into `complete.rs`: the property belongs
  to the emitted script, not to today's emitter, so this stays correct for a
  file written by any thegn version — which is what decouples this chunk from
  chunk 2.
- **`report()` / `report_with(env, exe, gen_script)`** — first existing file per
  (shell, command) wins; nothing found ⇒ one `Absent` row naming the _first_
  candidate as the destination. Every read is best-effort (a permission error
  reads as absent, never a doctor failure). Generation is lazy — it only runs
  when a file actually exists and lacks the marker, so a healthy or absent
  install pays nothing — and always into a `Vec<u8>`, because the generators
  panic on a write error.

`cmd/doctor.rs` gained ~60 lines and calls in; no logic inlined. The text
section, and the same rows mirrored into `--json` under `completions` as
`{shell, command, state, path}` via a `completions_json()` sibling of the other
per-section helpers.

Verified by hand against all four states, in an isolated `XDG_*`:

```
Completions
  6 installed and current (6 dynamic shims, which never go stale)
```

```
Completions
  zsh    thegn  dynamic  /tmp/…/data/zsh/site-functions/_thegn  (shim — asks the binary, never stale)
  zsh    tg     absent   — run: tg completions zsh > /tmp/…/data/zsh/site-functions/_tg
  fish   tg     stale    /tmp/…/cfg/fish/completions/tg.fish — run: tg completions fish > /tmp/…/cfg/fish/completions/tg.fish
```

A `--static` script installed by hand correctly reads `fresh` (it is diffed
against the `aot` generator); a shim with junk appended stays `dynamic`, which
is right — the appended junk cannot make it ask the wrong binary.

## Two judgement calls

- **`~/.zfunc` is in the zsh search list**, after the two the chunk spec named.
  `docs/cli.md` (chunk 3, already committed) tells a hand-installer to write
  `~/.zfunc/_thegn`; without this entry doctor would report the documented
  install as `absent`, which is the exact failure mode this section exists to
  prevent. Everything else is verbatim from the spec.
- **XDG fallbacks are `$HOME/.local/share` / `$HOME/.config`** when the vars
  are unset — which makes the spec's first and second bash entries (and first
  and second fish entries) the same path. They are de-duplicated, so no row is
  reported twice; a test pins that.

## One thing outside my lines

`just lint` was **red on this branch before this chunk**: `test/brand-guard.sh`
scans every tracked text file, and `.thegn/pipeline/architect/design.md:182`
(commit `185ba2f8`) named the pre-rename state root literally. Fixed in
`6876fc82` by rewording that clause — the migration code is the sanctioned
place to spell the old name and the sentence loses nothing. Flagging it because
it is not one of my four owned files, but the change cannot land with `lint`
red and I am the last coder here.

## Gates

| gate                                                   | result                                |
| ------------------------------------------------------ | ------------------------------------- |
| `just quick thegn-host`                                | clean                                 |
| `cargo clippy -p thegn-host --all-targets -D warnings` | clean                                 |
| `cargo nextest run -p thegn-host`                      | **2343 passed**, 8 skipped (11 new)   |
| `just smoke`                                           | all checks passed (3 new + PTY smoke) |
| `just lint`                                            | **exit 0** (after `6876fc82`)         |
| `just openspec-validate`                               | 166 passed, 0 failed                  |

e2e untouched and not run — `doctor` is a CLI verb, not a frame.
`main.rs` gained exactly one line (`mod completions_health;`).

## Not done here (deliberate)

- `just ci` — tasks 5.1/5.3 in the openspec change: the whole-change pre-PR
  gate (coverage, cross/feature/MSRV, `nix-build` for chunk 1's packaging,
  docs). Left unticked for whoever runs it once over the finished change.
- Task 3.5 (`/opsx:sync` the delta into `openspec/specs/`) and task 1.7 (build
  `.#default` / `.#dev` and check the completion outputs) belong to other
  chunks and are still open.
- The JSON carries exactly the four specified keys. The fix command is text-only
  today; if a consumer ever wants it, adding a `fix` key is additive.
