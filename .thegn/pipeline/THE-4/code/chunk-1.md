---
files:
  - CONTRIBUTING.md
  - README.md
  - docs/local-ci.md
  - docs/coverage.md
  - docs/testing-with-muse.md
  - docs/help/help.md
  - extensions/skills/tui-check/SKILL.md
  - extensions/skills/pipeline/SKILL.md
  - openspec/changes/document-dev-loop-policy/proposal.md
  - openspec/changes/document-dev-loop-policy/design.md
  - openspec/changes/document-dev-loop-policy/tasks.md
  - openspec/changes/document-dev-loop-policy/specs/architecture-gates/spec.md
  - openspec/specs/architecture-gates/spec.md
  - openspec/changes/archive/2026-08-29-document-dev-loop-policy/**
overlaps: []
after: []
---

# THE-4 chunk 1 — align dev-loop docs and close the OpenSpec record

## Goal

Make the already-landed dev-loop policy consistent across human contributor
docs, the bundled in-app help, and the two bundled skills that tell agents how
to test. Then reconcile the stale `document-dev-loop-policy` OpenSpec draft with
the branch, sync its corrected guard requirement into the canonical
`architecture-gates` spec, and archive the completed change. This is a prose,
markdown-asset, and OpenSpec-record change only.

## Files touched (exact paths)

- `CONTRIBUTING.md` — label the initial build and platform checklist as
  deliberate one-time/final validation; retain the platform-specific commands
  but remove any implication that they are per-edit iteration commands.
- `README.md` — annotate the command block with pre-push/CI tiers and point
  contributors to scoped `just quick [crate]` plus filtered package tests.
- `docs/local-ci.md` — replace the contradictory “single stage while iterating”
  block with the cheap scoped loop; explain that individual heavy stages are
  deliberate diagnostics/final gates and that `act` is not day-to-day testing.
- `docs/coverage.md` — correct the `just ci`/`just ci-local` e2e description and
  state that full coverage is deferred to the final gate, not paid per edit.
- `docs/testing-with-muse.md` — make the persistent session the iteration loop,
  use `cargo build -p thegn-host` when a fresh host binary is needed, and mark
  full `just e2e`/baseline updates as final intentional UI validation.
- `docs/help/help.md` — add a short contributor-facing in-app section naming
  `just quick <crate>`, filtered nextest, and the deferred heavy gates.
- `extensions/skills/tui-check/SKILL.md` — mirror the scoped build/session loop
  and explicitly defer full e2e until the UI change is settled.
- `extensions/skills/pipeline/SKILL.md` — add the coder's scoped check policy;
  preserve the existing report, monitor, concurrency, and cheap-ratchet
  instructions.
- `openspec/changes/document-dev-loop-policy/proposal.md` — replace stale
  “already consistent/no docs rewrite” claims with the verified residual scope,
  and state that no guard code changes are required.
- `openspec/changes/document-dev-loop-policy/design.md` — create the missing
  OpenSpec design artifact, summarizing the docs-only approach and validation.
- `openspec/changes/document-dev-loop-policy/tasks.md` — mark the reconciled
  work complete and replace the prohibited full `just ci` per-edit task with
  the scoped checks plus strict OpenSpec validation.
- `openspec/changes/document-dev-loop-policy/specs/architecture-gates/spec.md`
  — retain one accurate delta requirement for the existing guard; remove claims
  about matcher forms the current script does not actually cover.
- `openspec/specs/architecture-gates/spec.md` — sync the corrected delta here
  while preserving every existing requirement and scenario.
- `openspec/changes/archive/2026-08-29-document-dev-loop-policy/**` — the
  completed OpenSpec change after sync/archive; preserve its artifacts rather
  than leaving the active source directory in place.

Do not touch `CLAUDE.md`, `test/heavy-guard.sh`, `.claude/settings.json`,
`justfile`, `flake.nix`, Rust sources, config example, ratchet files, or control
API snapshots. They are either already correct or outside this documentation
residual.

## Approach

1. Read the current target passages and preserve their useful platform/TUI
   details. Change wording and command examples only where they distinguish the
   cheap iteration loop from the final gate.
2. In docs and help, use the same vocabulary everywhere: `just quick <crate>`
   for lib/bin clippy, `cargo nextest run -p <crate> <filter>` for touched
   tests, pre-push for the correctness gate, and CI/final UI validation for the
   expensive workspace-wide work. Do not claim that `just ci` runs e2e; the
   justfile makes that `just ci-local`.
3. In the TUI skill/guide, keep the isolated `muse` commands and cleanup rules.
   A scoped host build is acceptable when the executable is stale; the full
   `just e2e` suite and `just e2e-update` are explicit final validation actions,
   not commands after every small edit.
4. Repair the OpenSpec prose and create its missing design artifact. The delta
   must describe only behavior verified in `test/heavy-guard.sh`: recognized
   heavy invocations are refused with scoped alternatives, the explicit escape
   hatch passes through, malformed/missing dependencies fail open, and quoted
   or heredoc-only mentions are ignored. Do not add a code change to broaden the
   shell matcher in this issue.
5. Run the strict validator before archive. Sync the delta into the canonical
   architecture spec using the repository's `/opsx:sync` workflow, preserving
   existing requirements. Then archive the named change using `/opsx:archive`
   (current date `2026-08-29`), or perform the exact equivalent only if the CLI
   workflow is unavailable. Confirm the active source directory is gone and the
   dated archive contains the proposal, design, tasks, and delta spec.

## Overlap and dependency

This is the only THE-4 chunk. `overlaps: []` and `after: []`: it is independent
of every other coder chunk and has no sibling file collision. The OpenSpec sync
and archive are ordered within this chunk after the docs and validation, so no
other coder may begin from the active change directory during that operation.

## Tests to run (scoped)

```sh
just quick thegn-host
cargo nextest run -p thegn-host help mq_assets
openspec validate --all --strict
```

Do not run `just test`, `just ci`, `just coverage`, a full-workspace cargo
command, or e2e. The pre-push/CI owner runs those final gates once at the
appropriate boundary. `test/heavy-guard.sh` is not changed, so no new shell
behavior test is required.

## Done criteria

- All exact files above are either updated as specified or, for the final
  archive glob, moved intact into the dated archive.
- No contributor or agent testing instruction says to run `just lint`,
  `just test`, `just coverage`, `just ci`, or full `just e2e` after every edit;
  every iteration path offers the scoped commands.
- The README, contributor docs, local-CI guide, coverage guide, muse guide,
  in-app help, TUI skill, and pipeline skill agree on the same tiering.
- The OpenSpec proposal no longer claims the docs were already consistent; its
  design artifact exists; tasks are complete; the corrected guard delta is
  synced into `openspec/specs/architecture-gates/spec.md`; and the active
  change is archived under `openspec/changes/archive/2026-08-29-.../`.
- `just quick thegn-host`, the targeted `help`/`mq_assets` nextest invocation,
  and `openspec validate --all --strict` pass. Any unavailable tool or deferred
  full gate is reported, never hidden.
- The coder creates exactly one commit with this subject:

  `docs(the-4): align dev-loop guidance and archive OpenSpec change`
