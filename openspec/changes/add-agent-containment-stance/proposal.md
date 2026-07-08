# Add agent containment stance

## Summary

Record superzej's deliberate opsec / scope posture as enforceable requirements:
the conscious "decline" decisions surfaced by the 2026-06-28 Orca audit. This is
the **inverse** of Orca's host-exec-with-permission-bypass model — agents stay
**sandboxed-by-default** with no host execution absent an explicit, logged
`--no-sandbox` opt-out; the binary ships **no telemetry**; and superzej stays a
**viewer / VCS client** that hands editing off to `$EDITOR` rather than embedding
an editor, a browser, or desktop-automation (computer-use). These are not new
features — they affirm shipped behavior as a defended boundary and part of the
product's moat.

## Impact

- **AJ** (security / opsec), item **778** — records the agent-containment stance.
- Affirms **AJ 441** (no-telemetry / local-only default, done) and **AJ 443**
  (sandbox-by-default for agents, done); relates the sealed "Bouncer" agent and
  the LLM-proxy chokepoint.
- Adds a new **agent-containment** capability (policy requirements only; no code
  change required to satisfy — they lock current behavior).

## Rationale

The audit catalogued where superzej deliberately diverges from agent runtimes
that execute on the host with permission prompts bypassed, that phone telemetry
home, and that embed editors / browsers / computer-use. Each divergence was a
conscious decision, but a decision recorded only in narrative drifts. Encoding
them as normative SHALL / SHALL NOT requirements makes the posture testable, turns
a future regression (e.g. an agent quietly gaining host exec, or a telemetry
beacon, or an embedded editor) into a spec violation, and documents the moat for
contributors.

## Non-goals

- New enforcement code, config keys, or runtime behavior — these requirements
  describe and lock the existing posture.
- Removing the `--no-sandbox` escape hatch (item 362); it stays, but must be
  explicit and logged.
- Defining the Bouncer / LLM-proxy internals (owned by their own capabilities);
  this change only relates to them.
