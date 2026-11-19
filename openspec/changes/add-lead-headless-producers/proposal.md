# Lead headless producers

## Why

Rules fired only while a UI was open, so a Lead with no window saw nothing.
THE-2208 lets the daemon refresh the tracker, poll PRs and mentions, and drive
the PR queue, so rules fire with no window.

## What Changes

- `[daemon] headless_producers` (default false). With it on, one daemon task per
  armed producer kind (tracker, PR poll, mentions, PR queue) runs under a lease.
- A kind is armed only when something needs it (an enabled matching rule, an
  `[autopilot]` repo, `[pr_queue] enabled`, mention config).
- Schema v114: `producer_leases`. The UI yields while the daemon holds a lease
  and resumes after the TTL if the daemon dies.
- An armed producer keeps the daemon from idle exit.
- The PR poll covers at most 8 worktrees per tick.
- The daemon PR queue pass hands blockers to an agent only with
  `[pr_queue] agent_sandbox = true`; otherwise it notifies. `thegn lead pause`
  stops daemon PR queue work too.
- `thegn lead login [--mcp <server>]` (operator only).
- Fixed: the UI mention check runs every 5 minutes per repo.
- `thegn doctor` reports producer status.

## Impact

- **Roadmap**: item 769. Affected specs: `automation-rules`, `daemon` (new).
- Schema 113 to 114; restart the daemon after upgrading.

## Unverified

Nothing here has run live. See the unticked task in `tasks.md`.

- Lease acquire and take-over use a compare-and-swap, a lease whose holder died is
  taken over after its TTL, and the tracker producer keeps a memo of its last
  refresh (batch 163 amendment).
