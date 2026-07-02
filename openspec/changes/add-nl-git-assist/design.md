# Design

## The contract (core, pure parse)

The model is prompted to answer in an XML-tagged contract:

```
<command>git rebase -i HEAD~3</command>
<explanation>Combines the last 3 commits into one.</explanation>
<warning>Rewrites history; avoid if already pushed.</warning>  <!-- optional -->
```

A **pure** parser in `superzej-core` extracts `{ command, explanation, warning? }`
from the tagged response and maps `command` onto the typed `GitOp` where possible
— unit-tested: well-formed parse, missing optional warning, unmapped command
surfaces as "unrecognized" (not executed), malformed XML errors cleanly.

## The flow (host)

1. **Ask** — the user/agent's prose + minimal repo context (branch, staged state)
   goes to the model through the proxy (host-side call; the agent supplies prose,
   the host does the translation).
2. **Validate** — the parsed `GitOp` is pre-checked against the current repo state
   (e.g. rebase refused on the base branch, squash needs ≥2 commits, force-push
   flagged) so warnings are grounded, not just model-claimed.
3. **Confirm** — a confirm/warn overlay (the bouncer `ApprovalKind` overlay
   pattern) shows description + command + explanation + warnings with
   Confirm / Edit / Cancel. Edit lets the human tweak and re-validate.
4. **Execute** — on confirm, run via the existing `gitmut::execute()` (spawn_blocking).

An **explain-only** mode returns steps 1–2 (command + explanation + safety) and
never executes.

## Invariants

- **Event loop**: the model call and `execute()` run off-loop (proxy client /
  spawn_blocking), results over the channel + `TerminalWaker`; the confirm overlay
  is edge-driven input. No polling timer, no blocking git/LLM on the loop.
- **Render**: the confirm/warn overlay is a chrome `dirty` overlay repaint.
  render_plan invariants unchanged.
- **State**: no `user_version` bump — suggestion/confirm is transient.
- **Additivity**: the parser is pure core; the model call is the AI layer. Git ops
  are fully usable without the assist.

## Alternatives considered

- **Executing model output directly** — rejected; unsafe. The explain+warn+confirm
  gate and mapping to typed `GitOp` are the safety story.
- **Running proposals as raw shell** — rejected; mapping to `GitOp` validates
  against superzej's own ops and keeps the bouncer in charge of any shell.
- **Agent-side translation** — rejected; the host owns the translation + confirm so
  the safety gate is enforced regardless of which agent asked.
