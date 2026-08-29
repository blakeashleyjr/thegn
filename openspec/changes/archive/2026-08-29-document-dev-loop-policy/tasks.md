# Tasks

Iterate with `just quick <crate>` and targeted
`cargo nextest run -p <crate> <filter>`; reserve full gates for the final
pre-push/PR boundary.

## 1. Align contributor and agent guidance

- [x] 1.1 Update `CONTRIBUTING.md`, `README.md`, `docs/local-ci.md`, and
      `docs/coverage.md` with the scoped loop and accurate gate tiers.
- [x] 1.2 Update the muse guide, in-app help, TUI skill, and pipeline skill;
      keep persistent TUI sessions for iteration and defer full e2e until the
      UI change is settled.

## 2. Reconcile the OpenSpec record

- [x] 2.1 Correct the proposal's audit findings, scope, and roadmap impact;
      create the missing design artifact.
- [x] 2.2 Replace the stale tasks with completed documentation, validation,
      sync, and archive tasks.
- [x] 2.3 Record one accurate guard delta, sync it into the canonical
      `architecture-gates` specification, and archive this change under
      `2026-08-29-document-dev-loop-policy`.

## 3. Scoped validation

- [x] 3.1 Run `just quick thegn-host`,
      `cargo nextest run -p thegn-host help mq_assets`, and
      `openspec validate --all --strict`.
- [x] 3.2 Reserve one final `just ci` run, plus any needed coverage and e2e,
      for the pre-push/CI/final-UI boundary; do not run them per edit.
