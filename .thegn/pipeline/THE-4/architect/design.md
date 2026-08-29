# THE-4: Dev-loop documentation — architecture design

## Decision

THE-4 is substantially satisfied on this branch. The policy is already explicit
and mechanically enforced for AI agents; the remaining work is to remove a few
contradictory or under-specified instructions from contributor docs, bundled
testing guidance, and in-app help. This is a documentation/OpenSpec bookkeeping
change. It does not add runtime behavior.

The implementation is one file-disjoint coder chunk because all edits are
documentation, embedded markdown assets, and the existing OpenSpec record. No
parallel split would reduce risk or provide a useful seam.

## Audit and evidence

The repository standards require the architecture source and behavioral specs to
be kept current (`CLAUDE.md:30-37`, `docs/ARCHITECTURE.md:1-7`). The following
facts were checked against this branch:

- The canonical policy already tells contributors to use `just quick`, run
  targeted tests, and defer `just test`, `just coverage`, and `just ci`
  (`CLAUDE.md:163-205`). It also explains the pre-commit/pre-push/CI tiers
  (`CLAUDE.md:171-186`) and points AI agents at the `PreToolUse` guard
  (`CLAUDE.md:188-194`).
- The guard is already tracked and wired (`test/heavy-guard.sh:1-18`,
  `.claude/settings.json:2-11`). Its direct recipe set and scoped alternatives
  are in `test/heavy-guard.sh:51-87`; it fails open when `jq` or valid input is
  unavailable (`:20-23`), honors `THEGN_ALLOW_HEAVY=1` (`:25-28`), and blanks
  quoted/heredoc text (`:30-49`). No guard code or hook change is needed.
- Human contributor guidance is mostly correct (`CONTRIBUTING.md:53-59`), but
  its platform checklist presents `just build && just test && just smoke &&
just lint` without saying this is a one-time/final validation pass
  (`CONTRIBUTING.md:136-145`). The quick-start build is an initial build, not an
  iteration loop (`CONTRIBUTING.md:31-44`), and should merely be labeled as such.
- README guidance already says to iterate with `just quick` and defer heavy
  gates (`README.md:365-379`), but the command block labels `just test` and
  `just lint` generically (`README.md:366-372`). Add tier comments and a
  targeted-test example so the command list cannot be read as a per-edit loop.
- `docs/local-ci.md` directly contradicts the policy by calling `just lint` and
  `just test` “a single stage while iterating” (`docs/local-ci.md:12-24`). It
  must show `just quick [crate]` and a filtered package test as the iteration
  path, while retaining deliberate stage runs for debugging or pre-push/PR work.
- `docs/coverage.md` already has the right inner-loop/pre-push/CI tiers and says
  coverage is a full recompile (`docs/coverage.md:42-55`), but its CI row says
  `just ci` includes e2e even though the justfile puts e2e only in `ci-local`
  (`justfile:394-402`). Correct that stale row and make the “once at the end”
  wording explicit.
- The muse guide distinguishes the gate from the interactive loop
  (`docs/testing-with-muse.md:8-15`), but its “Quick start (the loop)” starts
  with the full-workspace `just build` (`:23-27`) and later requires `just e2e`
  twice for snapshot work (`:224-231`) without stating that e2e is a final,
  intentional gate. Use the crate-scoped host build for the interactive binary,
  tell readers to reuse the live `muse session`, and defer the full e2e suite to
  final UI validation.
- The bundled TUI skill repeats the same ambiguity: its setup starts with
  `just build` (`extensions/skills/tui-check/SKILL.md:15-30`) and its promotion
  step says to run `just e2e` twice (`:61-70`) without the defer rule. It needs
  the same scoped-build/session/final-gate language.
- `extensions/skills/pipeline/SKILL.md:353-369` already names package-scoped
  ratchet suites, but it does not state the general coder loop (`just quick
<crate>` plus filtered `cargo nextest`) or prohibit full gates per edit. Add
  that policy there; do not alter the pipeline's existing report/monitor rules.
- `docs/help/help.md:38-48` is the contributor-facing in-app help page and is
  the least surprising place for a short “keep the dev loop light” section.
  It has no action claim to add, so the help ratchets remain unchanged.

The existing OpenSpec proposal is stale in two ways: it says all contributor
docs are already consistent (`proposal.md:25-32`, disproved by
`docs/local-ci.md:15-18`) and says there will be “no docs rewrite”
(`proposal.md:41-50`, disproved by the audit above). Its requested
`openspec/.../design.md` is absent, although `proposal.md`, `tasks.md`, and the
delta spec are present. The coder chunk must repair that record before syncing
and archiving it.

The delta spec also must not claim more than the current shell matcher proves:
the direct guard recipes are listed at `test/heavy-guard.sh:56`, but the
special shell-runner matcher at `:64` has a narrower list. The synced wording
must describe the guard's recognized command forms and fail-open/quoted-text
behavior without inventing coverage for an unmodified matcher. This is a spec
accuracy correction, not a reason to expand the guard in this issue.

## Design and invariants

1. Keep `CLAUDE.md`, `test/heavy-guard.sh`, and `.claude/settings.json` as the
   already-landed policy/mechanism. Do not modify the hook, justfile, flake,
   Rust code, or test implementation.
2. Make every human-facing iteration recipe lead with the cheap path:
   `just quick <crate>` and `cargo nextest run -p <crate> <filter>`. Describe
   `just test`, `just lint`, `just coverage`, `just ci`, and `just e2e` as
   deliberate pre-push/pre-PR/final UI gates, not per-edit commands.
3. Keep TUI verification useful between edits: a crate-scoped host build only
   when the binary needs refreshing, then a persistent isolated `muse session`
   for look/act/look. The full `just e2e` snapshot suite is run once after the
   UI change is settled; `just e2e-update` remains an intentional baseline
   update followed by review.
4. Update only prose and the existing OpenSpec delta. There is no new config key,
   action, keybind, panel, provider, capability, schema, or runtime worker.
   Therefore the env-overlay, completion-slot, control-schema, and help-ratchet
   files do not change. The host asset/help tests still run to prove the edited
   markdown remains registered and valid.
5. Amend the proposal/tasks to reflect the audit, add the missing OpenSpec
   `design.md`, sync the corrected `architecture-gates` delta into
   `openspec/specs/architecture-gates/spec.md`, and archive the completed
   change at the current-date archive path. The archive is bookkeeping after
   the docs land; it must not leave an active duplicate change behind.

## Validation boundary

The coder runs only scoped checks:

```sh
just quick thegn-host
cargo nextest run -p thegn-host help mq_assets
openspec validate --all --strict
```

No `just test`, `just ci`, full-workspace compile, coverage, or e2e run belongs
in this implementation pass. The repository's pre-push hook/CI policy remains
the owner of those final gates, as documented in `CLAUDE.md:171-186` and
`CLAUDE.md:292-298`.
