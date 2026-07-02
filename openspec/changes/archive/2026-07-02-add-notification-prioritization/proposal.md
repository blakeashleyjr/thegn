# Add notification prioritization

## Summary

Give notifications a single source-of-truth priority model on `NotificationKind`
(three tiers: Alert / Notice / Info) that coherently drives the red attention flag,
the neutral unread count, and desktop-toast urgency — so informational events
(worktree created, process exited) stop raising the same red flag as a test failure.

Source design: `docs/superpowers/specs/2026-06-22-notification-prioritization-design.md`.

## Impact

- **AI** (Notifications) — priority model unifying the toast/badge/panel paths.
- Extends the `notifications` capability; no DB schema change (priority derived
  from `kind` at read time).

## Rationale

Priority is currently fragmented across three disagreeing mechanisms (toast
urgency, a hardcoded SQL alert-kind list, and an all-unread panel flag). Deriving
priority from `kind` at read time (with a config override) unifies them without a
`priority` column or a backfill, and lets a config remap reclassify historical rows
live.

## Non-goals

- A stored `priority` column (would freeze priority at insert).
- A second flag-threshold knob (per-kind remap gives full control).
