# Chunk 3 — done: contract and docs

Issue THE-36. Branch `tg/the-36-completions`. No code — spec artifacts and
documentation only.

## Commits (2, on the branch)

| commit        | what                                                                                  |
| ------------- | ------------------------------------------------------------------------------------- |
| `34ea119b`    | `docs(openspec)` — the `rework-shell-completions` change folder                       |
| `6f8a3a5a`    | `docs(completions)` — `docs/cli.md`, `docs/help/cli.md`, the extending recipe + index |
| _(this file)_ | the summary                                                                           |

## What landed

**`openspec/changes/rework-shell-completions/`** — validates strict
(`openspec validate rework-shell-completions --strict`; `--all --strict` is
166/166, up from the 165 baseline).

- `proposal.md` — design §1 compressed: nothing installs completions on any
  install path, the contract describes only a generator, and the industry
  default is wrong _here_ (per-pane shell spawn). Impact cites **THE-36**,
  Wave 3+ `completions/config/docs (THE-36/38/4)` (`tasks.md:185`), plus **A 6**
  (CLI surface v2, which created the verb) and **AO 494/495** (the install paths
  this completes).
- `design.md` — the three layers and who owns what, the `<TAB>` fast-path
  contract, the two unstable Cargo features with their containment boundary and
  Plan B, and **six rejected alternatives with reasons** (rc-file `eval`,
  static-only, carapace, per-target release copies, mtime staleness, a watchdog
  thread). Ends with the invariants section openspec's `design` rules ask for:
  render damage channel none, no schema change, no help-context key.
- `tasks.md` — five groups: one per chunk (1 delivery, 2 engine, 3 contract+docs,
  4 health) plus validation, ending in the run-once `just ci` gate. State marked
  honestly at the tip: groups 1 and 2 checked except `1.7` (the `nix build`
  output verification chunk 1 left UNVERIFIED); group 3 checked except the
  `/opsx:sync` + archive step; group 4 and validation open.
- `specs/cli/spec.md` — **1 MODIFIED** (the existing completions requirement,
  replaced: keeps generated-from-the-live-tree + invoked-binary-name, adds that
  the default output is a registration script resolving candidates at completion
  time and that `--static` emits the self-contained one) and **4 ADDED**:
  packager-owns-delivery (both names, three shells, cross skips, one release
  asset, **no rc-file `eval` in the documented instructions**); the bounded /
  never-mutates contract (no config load, no read-write DB, no network, exit 0
  silent on any failure, `--profile` honoured, lazy sources, deadline); sources
  implemented-or-`reserved` with the drift ratchet; freshness diagnosable in
  `thegn doctor`.

**Docs.**

- `docs/cli.md` `## Completions` — rewritten from two lines: packaged installs
  are already done; a per-shell table with the real destination path for
  everyone else (plus the `mkdir`/`fpath` prerequisite a new user actually hits);
  the **do not `eval` in your rc** paragraph with the per-pane reason; staleness
  and `thegn doctor`; what completes, and that branch/PR/issue deliberately do
  not.
- `docs/help/cli.md` — the same in the in-app voice, on the existing
  `completions` bullet. No frontmatter change: no action ids or keybinds are
  claimed.
- `docs/extending/completion-source.md` (new) + the `docs/extending/README.md`
  index row — `cli-subcommand.md`'s shape: numbered steps (declare the
  `SourceKind`, serve it in the right source family, add the `CATALOG` row,
  unpin the ratchet line, respect the fast-path contract, test to the 95% gate)
  and a bold **Gates:** line.

## Verification

- `openspec validate --all --strict` — 166 passed, 0 failed.
- `cargo nextest run -p thegn-host help` — 71 passed; `… ratchet` — 12 passed
  (the four help ratchets among them). Nothing new is claimed, so nothing moved.
- `cargo nextest run -p thegn-host mcp` — 5 passed (`docs/cli.md` is
  `include_str!`'d into the MCP docs corpus).
- `treefmt` clean on every file touched. `just lint`/`just ci` deliberately not
  run per the dev-loop policy — this change is markdown only, and treefmt is the
  only part of `lint` that applies to it.
- Read the `docs/cli.md` section as a `cargo install` user: shell table → one
  command → the directory prerequisite → done, without reaching for the `eval`.

## Two things the Lead should know

1. **`docs/cli.md` and `docs/help/cli.md` claim `thegn doctor` reports
   `fresh`/`stale`/`absent` — that is chunk 4, which has NOT landed.** Chunk 3's
   merge-order gotcha covers chunk 2 (landed, so the `--static` and live-value
   sentences stand as written) but the same rule applies here: if this branch
   ships without chunk 4, drop the "Staleness" paragraph's last sentence in
   `docs/cli.md` and the `thegn doctor` clause in `docs/help/cli.md`. Everything
   else is true from chunks 1 + 2 alone. The delta spec's freshness requirement
   should ship with chunk 4 either way — it is the contract chunk 4 implements.
2. **`openspec/changes/package-shell-completions` is a live change folder for
   the same issue** (committed in `2dbdd588`, from the THE-board sweep) and it
   `MODIFIED`s the same `cli` requirement with contradictory text — it documents
   the rc-file `eval` and defers live values. My proposal opens with a
   `SUPERSEDES` block saying so, in the shape `add-pipeline-board` uses, but I
   did not touch that folder: it is outside this chunk's file set. **Archive it
   at `/opsx:sync` time** (task 3.5 records this). `validate --all --strict`
   does not object to two in-flight deltas on one requirement, so nothing
   catches it mechanically.

## Files touched

`openspec/changes/rework-shell-completions/{proposal,design,tasks}.md` and
`specs/cli/spec.md` (new); `docs/cli.md`; `docs/help/cli.md`;
`docs/extending/completion-source.md` (new); `docs/extending/README.md`.

Nothing under `crates/`, `nix/`, `test/`, `.github/`, or `openspec/specs/`.
