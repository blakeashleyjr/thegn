---
id: automations
title: Automations
order: 33
parent: configuration
actions: []
---

# Automations

Automations are trusted, typed `when … if … then …` rules. They are loaded only
from the global config and its active profile; a repository `.thegn.*` file may
not install persistent actions.

Use `thegn automations list` to inspect the effective rules, whether each rule
is active, and its most recent audited outcome. Add `--json` for one stable JSON
array.

Use `thegn automations test <rule> --fixture event.json` (or `--event '<json>'`)
to evaluate one rule against a normalized event. This is a pure dry run: it does
not execute an action and does not open or write the live state database. The
optional `--at <unix-seconds>` supplies the evaluation clock; otherwise the
fixture's `occurred_at` is used.

Rules can react to notifications, agent attention/completion/failure, PR facts,
landed merges, worktree idleness, and disk pressure. V1 actions are the catalog
capabilities `sessions.open`, `merge.add`, `notify.push`, and `tools.run`.
Configured names (`agent` and `name`) are resolved from trusted config. Event
data can fill validated text templates, but never becomes an executable name,
capability id, or shell fragment.

Every live run is bounded by `[automations] max_concurrent`, `queue_capacity`,
and `action_timeout_secs`. Outcomes remain visible as audit rows and as
`automation` / `automation_failed` inbox notifications. Events caused by an
automation carry an origin marker and remain visible, but cannot trigger more
automation rules.
