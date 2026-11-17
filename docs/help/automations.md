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

Event enrichment matches the registered worktree by exact path, then by
resolved filesystem identity, so a symlinked path or one with `.`/`..`
components enriches the same way. If that alias matches worktrees in more than
one repository, the repo, branch and workspace are left empty.

## Rules written by the Lead

The Lead can author its own automation rules through the `automations.put`,
`automations.rm`, `automations.enable` and `automations.runs` verbs (also
`thegn api call automations.put '{"rule":{…}}'`). The rule body is the same
shape as an `[[automations.rules]]` table and goes through the same validator.
Action parameters may also be nested as `then.params`. Unknown fields are
refused rather than ignored.

These rules are deliberately weaker than config rules:

- The id is always `lead:<name>`, where the name is 1 to 48 characters of
  `a-z`, `0-9`, `_` and `-`. A name that matches a config rule is refused.
- The rule may only `notify.push` or `sessions.open`. A rule that uses
  `merge.add` or `tools.run` is refused when it is written. It is also skipped
  every time rules are evaluated, so a row written straight into the database
  cannot escalate. Approval-gated escalation comes in a later release.
- `max_per_hour` and `max_action_per_hour` are capped at the config defaults.
- Removing a rule and writing it again keeps its throttle state and audit
  history.
- At most 500 Lead rules can be stored.

Config rules stay read-only: `automations.rm` and `automations.enable` on a
config rule name answer a conflict. `thegn api call automations.list` shows Lead
rules after the config rules with the trusted layer `lead`, and the reply to
`automations.put` says why a stored rule will not fire yet (`inert_reason`):

- automations are disabled;
- the runtime is not armed (no config rules and no `[lead]` section);
- the rule is a `worktree_idle` rule (these arm in a later release);
- the rule is disabled.

`thegn api call automations.test` dry-runs a Lead rule by name. `thegn
automations test` and `thegn automations list` read config only.
`automations.runs` returns the newest audited runs, up to 200.
