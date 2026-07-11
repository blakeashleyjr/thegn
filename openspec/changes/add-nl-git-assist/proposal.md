# Add natural-language git assist (explain + warn + confirm)

## Summary

Add a natural-language → git action, modeled on
[`lumen`](https://github.com/jnsahaj/lumen)'s `operate`: the user (or an agent)
describes an intent in prose ("squash the last three commits", "undo my last
merge"), thegn proposes the concrete git operation with an **explanation** and
any **danger warnings**, and executes only after an explicit **y/N confirm**. The
model returns an XML-tagged `<command>/<explanation>/<warning>` contract that maps
onto thegn's existing typed `GitOp` surface.

## Impact

- **T 266** (AI change explanation) / **T 269** (PR creation from review) — the
  explain-before-act contract is reusable for describing/authoring git actions.
- **Merge queue / fold actor** — the same `<command>/<explanation>/<warning>`
  contract can front the merge/fold actor's AI assist.
- Extends the `git-backend` and `agent` capabilities. **No DB schema change** —
  suggestion + confirm is transient; execution uses the existing `GitOp` path.

## Rationale

thegn already has a complete typed git mutation surface (`gitmut.rs` `GitOp`
with 100+ ops + `execute()`), template expansion (`custom_cmd`), a semantic layer
that suggests commit messages, and the proxy for the model call — plus the
bouncer's approval-overlay pattern to model a confirm gate. lumen shows the safe
shape: never run NL directly; translate it to a concrete command, **explain it,
warn on danger, and require confirmation**. Mapping the model's proposal onto the
typed `GitOp` enum (rather than executing raw shell) means the proposal is
validated against thegn's own operations and can be pre-checked for safety
before the user confirms. Git remains fully usable without this — it is an
additive assist over the existing ops.

## Non-goals

- **Executing without confirmation** — a proposed operation always requires an
  explicit confirm (or an explain-only mode that never executes).
- **Replacing the git ops UI** — this is an assist that produces a `GitOp`; the
  lazygit-style ops and keybinds are unchanged.
- **Free-form shell execution** — the proposal is mapped to the typed `GitOp`
  surface, not run as arbitrary shell (the bouncer still governs any shell path).
- **AI-free-shell dependency** — all git operations remain available without the
  assist; it is an additive AI-layer convenience.
