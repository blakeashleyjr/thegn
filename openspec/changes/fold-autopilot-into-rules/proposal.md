# Fold autopilot into rules

## Why

`[autopilot]` ran as a separate UI-process supervisor outside the rule budgets
and kill switch. THE-2196 expresses it as an automation rule.

## What Changes

- New config-only action `autopilot.run` (`issue_matched` only, capped at
  `review`; a Lead-authored rule naming it is refused).
- An enabled `[autopilot]` repo synthesizes an `autopilot:<repo>` rule.
- Budgets count autopilot runs and the Lead's manual spawns.
