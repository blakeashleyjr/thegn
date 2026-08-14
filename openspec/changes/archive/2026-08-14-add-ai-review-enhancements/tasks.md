# Tasks

## 1. Annotate-AI-diff batch loop (T 763)

- [ ] 1.1 — Add a pinned-annotation model in `thegn-core` keyed by
      `(path, hunk-anchor)` with re-anchoring on diff refresh and drop-on-missing,
      pure and unit-testable. **unit tests** for anchor survival across an edit and
      drop when the hunk disappears (95% core gate).
- [ ] 1.2 — Persist annotations in a new cache table; bump SQLite `user_version`.
      **unit tests** for round-trip persistence and rehydrate after restart
      (isolated `XDG_STATE_HOME`).
- [ ] 1.3 — Render annotations in the existing diff/review pane and implement
      "send all to agent" as one ACP `session/prompt` (host-side). A pin add/remove
      is a chrome change ⇒ `Full` only on transition; keep render-plan invariants
      green.
- [ ] 1.4 — AI-off fallback: annotations remain a plain review-note list and the
      agent-send action is disabled with no proxy call. **unit tests** for the
      send-enabled-vs-disabled gating on AI presence.

## 2. AI commit-message draft (T 764)

- [ ] 2.1 — Add commit-message drafting that sends the staged diff through
      `thegn-proxy` and pre-fills the commit editor (no auto-commit; user
      confirms). **unit tests** for the prompt assembly from a staged diff.
- [ ] 2.2 — AI-off fallback in `gitmut.rs`: open the editor with the deterministic
      template/empty body; hooks run normally and `--no-verify` is untouched.
      **unit tests** for the template fallback and that no `--no-verify` is emitted.

## 3. "Fix with AI" on failed checks (T 765)

- [ ] 3.1 — Assemble a repair prompt from failing pre-commit hook output, and from
      `CiRun` failed-job names + `CiLog::first_failure_line`, then hand it to the
      embedded agent; re-run the check/hook after edits (never `--no-verify`).
      **unit tests** for repair-prompt assembly from both hook and `CiRun`/`CiLog`
      inputs.
- [ ] 3.2 — AI-off fallback: the action shows the failing output (hook text /
      first-failure line) in the pane with no repair prompt and no proxy call.
      **unit tests** for the show-output-only path when AI is absent.

## 4. Image-diff modes (T 766)

- [ ] 4.1 — Detect changed images supported by the graphics preview path and add
      swipe / onion-skin modes rendering old (`HEAD`) vs new (working) blobs in the
      diff pane; unsupported formats fall back to the normal text/binary diff.
      **unit tests** for mode state and the supported-format gating (AI-free).
- [ ] 4.2 — Wire mode toggle in the diff pane (host-side); a mode switch is a
      diff-pane chrome change ⇒ `Full` only on transition, redraw confined to the
      pane region; keep render-plan invariants green.

## Validate

- [ ] Run `just ci`
