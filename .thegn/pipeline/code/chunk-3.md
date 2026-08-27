# Chunk 3 — Contract and docs: the spec says what we actually do

**Issue:** THE-36 (right layer for shell completions).
**Design:** `.thegn/pipeline/architect/design.md` — §1 is the argument you are
writing down; §§4–6 are the details you are pinning.
**No code.** Spec artifacts and documentation only.

## Why

thegn's own development is spec-driven (`CLAUDE.md` → _Spec-driven development
(OpenSpec)_): `openspec/specs/<capability>/spec.md` describes how the system
behaves **today**, and each in-flight change is a self-contained folder under
`openspec/changes/`. Today the completions contract is a single requirement with
one bash scenario (`openspec/specs/cli/spec.md`, _"Shell completions are
generated from the CLI definition"_) — it describes a generator and says nothing
about who installs the output, what a `<TAB>` may cost, or what it may never do.
Those last two are the load-bearing parts of this change; if they are not in the
spec they will regress.

The user-facing docs are similarly thin — one line in `docs/cli.md:216`, one in
`docs/help/cli.md:136` — and neither tells anyone how to actually get
completions working. Worse, the obvious thing for a user to reach for (the
industry-default `eval "$(thegn completions zsh)"` in their rc) is the one
option this design rejects, for a reason specific to thegn: it is a multiplexer
that spawns a shell per pane, so an rc-file `eval` puts a `thegn` process launch
into every pane restore. The docs need to say that, and say why.

## Files you own

- `openspec/changes/rework-shell-completions/` (new: `proposal.md`, `design.md`,
  `tasks.md`, `specs/cli/spec.md`)
- `docs/cli.md` (the `## Completions` section, currently ~line 214)
- `docs/help/cli.md` (the completions bullet, currently ~line 136)
- `docs/extending/completion-source.md` (new)
- `docs/extending/README.md` (index entry)

Do not touch `crates/`, `nix/`, `test/`, `.github/`, or
`openspec/specs/` — the main specs are synced from a change folder by
`/opsx:sync` after implementation, not hand-edited here.

## Approach

### 1. The openspec change folder

Name it `rework-shell-completions`. Copy the structure from an existing change
(`openspec/changes/add-pipeline-board/` is a recent, well-formed one) and follow
`openspec/config.yaml`'s schema. `just openspec-validate` (`openspec validate
--all --strict`) runs in `just ci` and must pass.

**`proposal.md`** — the argument from design §1, compressed. Say what is broken
(nothing installs completions on any install path; the contract describes only a
generator), what changes (delivery moves to the packager; the installed artifact
is a shim that calls back into the binary; a `<TAB>` gets a hard contract), and
why the industry default is wrong here (the per-pane shell spawn). Impact must
cite **THE-36** and the `tasks.md` roadmap item that carries it — group AX /
Wave 3+, the line reading `completions/config/docs (THE-36/38/4)`
(`tasks.md:185`).

**`design.md`** — the decision record. Do not re-derive it; distil
`.thegn/pipeline/architect/design.md` §§1, 4, 6: the three layers and who owns
what, the `<TAB>` fast-path contract, the two unstable Cargo features with their
containment boundary and the specified Plan B fallback, and the rejected
alternatives (rc-file `eval`; a third-party completion framework such as
carapace; static-only). Rejected alternatives with reasons are the part a future
reader needs most.

**`tasks.md`** — the implementation checklist, grouped so each group maps to one
of this change's four chunks. Mark the state honestly at the time you write it.
Include the final "run `just ci`" validation task — a **pre-PR gate run once**,
per `CLAUDE.md`.

**`specs/cli/spec.md`** — the delta. Use `## MODIFIED Requirements` for the
existing completions requirement (it is being replaced, not extended) and
`## ADDED Requirements` for the new ones. Behaviour-first, `SHALL`/`MUST`, with
`#### Scenario:` WHEN/THEN blocks. Cover at minimum:

- **MODIFIED — Shell completions are generated from the CLI definition.** Keep
  the existing guarantee (generated from the live clap definition, named from
  the invoked binary name) and add: the default output is a registration script
  that resolves candidates from the binary at completion time; `--static` emits
  a self-contained script generated from the command tree.
- **ADDED — Completions are installed by the packager, not the user's shell rc.**
  A packaged install SHALL place completion files for both the `thegn` and `tg`
  names into the platform completion directories for bash, zsh and fish; the
  documented install instructions SHALL NOT require an rc-file `eval`, because
  thegn spawns a shell per pane and an rc-file `eval` would add a process launch
  to every pane restore.
- **ADDED — A completion request is bounded and never mutates state.** MUST NOT
  create, migrate, or write to the state DB; MUST NOT perform network or forge
  I/O; MUST exit 0 with no output on any error, timeout or panic; MUST NOT print
  warnings, errors or backtraces; MUST honour `--profile`.
  Scenario: _WHEN a completion is requested with an empty state directory THEN
  the process exits 0 and the directory is still empty._
- **ADDED — Completion value sources are implemented or reserved.** Each slot in
  the CLI tree SHALL be bound to a value source, declared structural, or pinned
  in a shrink-only ratchet; a source kind is either implemented or `reserved`
  with a recorded reason.
- **ADDED — Completion freshness is diagnosable.** `thegn doctor` SHALL report,
  per shell, where completion files are installed and whether they are current.

### 2. `docs/cli.md` — rewrite the `## Completions` section

Replace the two-line section. It should tell a user, in order:

1. **If you installed a package** (Nix, or the release completions asset):
   completions for `thegn` and `tg` are already installed for bash, zsh and
   fish. Nothing to do.
2. **Otherwise** (`cargo install`, a bare binary): the one command per shell that
   writes the file to the right place — bash
   (`~/.local/share/bash-completion/completions/thegn`), zsh (a directory on
   `fpath`, file named `_thegn`), fish
   (`~/.config/fish/completions/thegn.fish`), plus elvish and PowerShell, which
   have no standard location and are generated on request only.
3. **Do not put `eval "$(thegn completions zsh)"` in your rc.** Say why in one
   sentence: thegn spawns a shell per pane, and warm reattach restores many at
   once, so an rc-file `eval` puts a `thegn` launch into every pane restore. This
   is the single most useful sentence in the section — a reader who copies the
   pattern from `gh` or `rustup` needs to be stopped here.
4. **Staleness:** an installed file is regenerated with the binary by any real
   package, so it cannot drift; for hand-installed files, `thegn doctor` reports
   `fresh` / `stale` / `absent` and prints the command that fixes it.
5. What gets completed: verbs and flags always; live values (worktrees, repos,
   sessions, hosts, config keys, capability ids) when the installed script is
   the dynamic one. Note that branch, PR and issue completion are deliberately
   not offered — a `<TAB>` does not run git or call the forge.

`docs/cli.md` is embedded in the binary and served over MCP, so keep it terse
and factual.

### 3. `docs/help/cli.md`

One or two lines, matching the in-app help voice: completions are installed by
your package manager; `thegn completions <shell>` for everything else;
`thegn doctor` tells you if they are stale. The help corpus has ratchets
(`crates/thegn-host/src/help/ratchet_tests.rs`) — you are adding no action ids
and no keybinds, so nothing new should be claimed in the page frontmatter; run
`just test -p thegn-host help` (or `cargo nextest run -p thegn-host help`) to
confirm the help ratchets stay green.

### 4. `docs/extending/completion-source.md`

A recipe in the exact shape of the siblings (`cli-subcommand.md` is the closest
model: numbered steps, then a bold **Gates:** line). Steps: add the
`SourceKind` variant; implement `CompletionSource`; add the `CATALOG` row
binding the (command path, arg id) slot; remove the slot's line from
`test/completion-slot-ratchet.txt` if it had one; unit-test to the 95% core
gate; note the fast-path contract the source must respect (bounded, read-only,
no network, fail-open, lazy). Gates: the slot-drift test, `just coverage`,
`test/smoke.sh`.

Add the index entry to `docs/extending/README.md` in the existing style.

## Tests / verification

- `just openspec-validate` passes (`openspec validate --all --strict`).
- `cargo nextest run -p thegn-host help` — help ratchets green.
- `just lint` — treefmt covers markdown formatting; match the surrounding style.
- Read your own `docs/cli.md` section as a new user with a `cargo install`d
  binary and confirm you could get completions working from it alone.
- Do **not** run `just ci` per edit (`CLAUDE.md` dev-loop policy).

## Done criteria

- `openspec/changes/rework-shell-completions/` exists, validates strict, and
  its `tasks.md` maps groups to the four chunks of this change.
- The delta spec pins the fast-path contract — no state mutation, no network,
  fail-open, `--profile` honoured — and the packager-owns-delivery requirement.
- `docs/cli.md` tells a user how to get completions on every install path and
  explicitly warns against the rc-file `eval`, with the reason.
- `docs/extending/completion-source.md` exists and is linked from the index.
- No file outside the ones you own is modified.

## Gotchas

- **Merge order.** Your docs describe the finished change. If chunk 2 (the
  dynamic engine) has not landed when this merges, drop the two sentences that
  mention `--static` and live values — everything else in your text is true
  from chunk 1 and chunk 4 alone. Check before you push.
- Do not hand-edit `openspec/specs/cli/spec.md`. Main specs are synced from the
  change folder after implementation (`/opsx:sync`), which is a separate step.
- `tasks.md` (the repo root roadmap) stays the map — cite THE-36 from the
  proposal's Impact rather than expanding the roadmap entry.
- The keybindings and config-reference help pages are generated at runtime;
  never hand-write those.
