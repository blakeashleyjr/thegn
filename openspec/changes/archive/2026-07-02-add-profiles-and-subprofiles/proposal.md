# Add profiles and subprofiles

## Summary

Introduce two levels of isolation. A **profile** is a complete firewall over the
entire program (separate OS process + scope root): distinct state/DB, config,
theme, credentials + git identity, and sandbox/network policy — "Work" vs
"Personal". A **subprofile** scopes a single subsystem inside a profile
(in-process), inheriting everything else — e.g. keep development unified while
splitting Comms into work/personal accounts.

Source design (design-only): `docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`.

## Impact

- **H** (Profiles & subprofiles, items 101–110, 536–539) — the headline feature.
- **AM** (item 540) — Comms is the first subprofile consumer.

## Rationale

superzej is one process / one world today: a single config, DB, credential set,
theme. The roadmap flags profiles as cross-cutting and "seed early." The codebase
is already env-driven (path roots, sandbox env-passthrough, `gh` token all read
`std::env`), so the firewall is best enforced by rerooting a profile-scoped
process environment once, as the first statement in `main`.

## Non-goals

- Multiple windows focus management beyond spawn + best-effort X11 focus
  (Wayland cannot focus foreign windows).
- Building Comms itself here — only the subprofile mechanism it will consume.
