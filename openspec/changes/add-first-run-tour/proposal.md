# Add the first-run welcome tour

Roadmap: public launch 2026-11-30. No Linear issue was supplied.

## Why

A new user's first minute decides whether thegn sticks. Today the first launch
opens the setup wizard (`thegn setup`): a 72-column form that asks for forge
auth, issue trackers and ssh hosts before the user has seen a worktree. It
never explains the workspace model, never says what the sandbox tiers protect,
and never asks which coding agent to use. Someone who just wants a shell has to
read the form to find out how to leave it.

## What Changes

- A full-screen, eight-chapter **welcome tour** replaces the wizard as the
  first-run surface: welcome, the workspace model (a small interactive
  diagram), look (live theme preview + keymap preset), sandboxes (an honest
  tier table, the backends that actually work on this machine, a default
  tier), agents (installed harnesses detected off the loop, a default agent),
  remote offload (explained, never configured inline), a key drill (press the
  real chord, watch the diagram react), and finish (the exact config diff,
  then three concrete next steps).
- Every chapter is skippable (`Tab`); `Esc` leaves the whole tour at once and
  writes nothing. Choices are written only on the finish screen, through the
  existing `config_write` funnel, off the event loop.
- The palette's `setup-wizard` row opens the tour; the tour's welcome and
  finish chapters hand off to the classic setup wizard (forge, trackers,
  hosts, paths) with `a`. `thegn setup` still opens the classic wizard.
- New `config_write::promote_agent` makes an agent the default (first
  `[[agents]]` entry) without dropping or rewriting the others.

## Decisions stated loudly

- No animation at all. The tour is static by design, so reduced-motion and
  `THEGN_E2E` need no special path. Delight comes from the live theme preview
  and the drill, not from motion.
- Status is never colour-only: every state carries a word and a glyph that
  degrades through `caps::active_glyphs()` (ASCII terminals, `NO_COLOR`).
- The tour does not ask for tokens. Remote offload is explained with the
  commands and help pages to use later.
- Choosing an agent that is not installed is refused, not written.

## Impact

Specs: onboarding (new capability).
Code: `crates/thegn-host/src/welcome.rs` (new), `onboarding.rs`,
`handlers/onboarding.rs`, `thegn-core/src/config_write.rs`.
Docs: `docs/help/onboarding.md` (new), `getting-started.md`,
`command-palette.md`. Tests: unit tests for the state machine and the agent
writer; muse spec `35-onboarding.yaml`.
