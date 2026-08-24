# Spec the dev-loop heavy-gate guard (THE-4 is otherwise done-by-policy)

Linear: THE-4

## Why

THE-4 asks to "update documentation to spare testing until done with
everything, to spare system CPU". The audit finds the substance of this
**already done — and mechanically enforced**, not just documented:

- **CLAUDE.md** carries the dev-loop policy in full ("Dev-loop policy — don't
  peg the machine" + "Test precisely; keep full-workspace rebuilds to an
  absolute minimum"): iterate with `just quick <crate>` / scoped `nextest`,
  run the heavy gates once at push time, let the pre-push hook be the thing
  that runs them.
- **`test/heavy-guard.sh`**, wired as a `PreToolUse` hook in
  `.claude/settings.json`, makes the policy mechanical for AI agents: the
  full-workspace gates (`just test|test-doc|ci|ci-local|coverage|
coverage-html|lint|bench|bench-micro|e2e|doc-check`, `cargo llvm-cov`,
  `--workspace` cargo runs — including behind `nix develop --command` and
  `sh -c` wrappers) are refused with a pointer to the scoped equivalents;
  `THEGN_ALLOW_HEAVY=1` is the deliberate pass-through; the guard fails open
  (no `jq`, unparseable payload) and never fires on gate names inside quoted
  strings or heredocs.
- **CONTRIBUTING.md** teaches the same tiers to humans (iterate `just quick`;
  `just test`+`just smoke` before push; `just ci` once before a PR).
- **`openspec/config.yaml`** bakes it into every generated change: the tasks
  rule ends each change with _one_ final `just ci` run, and a
  CLAUDE.md note marks that task explicitly as a run-once pre-PR gate.
- The dev shell caps `CARGO_BUILD_JOBS` and wires sccache;
  `docs/extending/*` contains no per-edit heavy-gate advice to contradict any
  of it.

The one genuine gap: the guard is load-bearing repo behaviour that exists
only as an untracked-by-spec shell script plus prose. Every other enforced
invariant in this repo has a spec'd gate (`architecture-gates`); the dev-loop
guard has none, so deleting or de-fanging it would violate nothing. This
change closes THE-4 by adding that one missing artifact — a spec requirement
describing the guard's current behaviour — and changes no code.

## What Changes

- **Spec only**: `architecture-gates` gains an ADDED requirement describing
  the heavy-gate guard as it behaves today (refusal + scoped-equivalent
  pointer, `THEGN_ALLOW_HEAVY=1` pass-through, fail-open on a broken guard,
  quoted-mention immunity, git hooks unaffected).
- **No code, no docs rewrite**: CLAUDE.md / CONTRIBUTING.md / config.yaml are
  already consistent; one small implementation task reconciles the guard's
  recipe list against the justfile's current heavy recipes so the spec is
  true on day one (e.g. decide whether `bench-idle` belongs in the list).

## Non-goals

- Changing the policy itself, the hook tiers, or the pre-push gate.
- Guarding human shells or git hooks — the guard deliberately covers only the
  AI-harness `PreToolUse` seam; git hooks are the correctness gate and run
  outside it.
- Re-litigating the CI-off / pre-push-only posture (documented in CLAUDE.md,
  out of scope here).

## Impact

- Roadmap: no existing tasks.md item covers this dev-process concern (closest
  is the AO Onboarding/DX group); the audit phase can wire it where it sees
  fit.
- Specs: `architecture-gates` — 1 ADDED requirement. Note the deliberate
  scope widening: that spec's existing framing is "gates that run in
  `just lint`/`just test`"; this requirement documents a _harness_ gate, and
  says so.
- Code: none. `test/heavy-guard.sh` may gain/lose a recipe name during
  reconciliation (task 1.1); `.claude/settings.json` unchanged.
- In-flight reconciliation: none — no other change touches the dev-process
  gates.
- Capability catalog, event loop, help ratchets, coverage: all untouched
  (nothing user-facing changes).
