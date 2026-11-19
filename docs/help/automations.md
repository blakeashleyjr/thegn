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
is active, its autonomy (with a note when policy capped it), its schedule, and
its most recent audited outcome. A banner shows when automations are paused. Add `--json` for one stable JSON
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
- The rule may only use `notify.push`, `sessions.open` and `lead.wake`, plus
  `pipeline.start` at autonomy `propose` (which raises an approval). A rule that
  uses `merge.add`, `merge.drain`, `tools.run`, `merge` autonomy or any other
  action is refused when it is written. It is also skipped every time rules are
  evaluated, so a row written straight into the database cannot escalate.
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

## Managing stored rules from the CLI

- `thegn automations add --file rule.toml` (or `--json '<rule>'`) stores one
  rule through the daemon. The file is TOML shaped like one
  `[[automations.rules]]` entry, or JSON when it ends in `.json`; it is read
  with a 64 KB limit. The reply gives the id and any `inert_reason`.
- `thegn automations rm <name>` removes a stored rule. Config rules are
  read-only.
- `thegn automations runs [rule] [--limit N] [--json]` shows recent audited runs:
  time, rule, trigger, action, outcome, autonomy and error.
- `thegn automations pause` and `resume`: see Pause and resume below.

CLI-added rules are Lead-class: they use the restricted action list, can never
be `merge`, and may use `pipeline.start` only at `propose`. Trusted unattended
rules (`pipeline.start` at `review` or `merge`, `merge.add`, `merge.drain`,
`tools.run`) belong in `[[automations.rules]]` in config.toml. Letting an
operator store trusted rules through an Admin put is planned.

## Schedules

A rule with `when = "schedule_due"` sets exactly one of:

- `schedule = "<5-field cron>"`: minute, hour, day of month, month, weekday.
  Fields take `*`, lists, ranges, steps, and month and weekday names. It is
  evaluated in the host's local time zone. A local time that does not exist
  (spring forward) is skipped; an ambiguous one (fall back) fires once, at the
  earlier instant.
  `@hourly`-style macros and `@reboot` are refused; with both day of month and
  weekday restricted, either matching fires (Vixie); 0 and 7 are Sunday.
- `every = "<n>s|m|h|d"`: between 60 seconds and 7 days, counted from when the
  producer starts.

There is no catch-up: nothing is persisted, and the next due time is always
computed from now, so a missed run is not replayed. A run is skipped while an
earlier one from the same rule is still live. The normal per-rule rate cap
(`max_per_hour`) applies.

Schedules are produced by the daemon only. With `[daemon] enabled = false` they
are inert, and `thegn automations list` says `schedule_due needs the daemon`. An
armed schedule keeps the daemon from idling out.

## Issue triggers

`when = "issue_matched"` fires when an issue from the tracker newly matches the
rule's `if` fields (for example labels, state, assignee and title text; the
issue title is the `message`). It fires when an issue appears or changes status,
labels, assignees or project while the rule exists; an unchanged issue never
fires when a rule is added, the first refresh of a repo only records a baseline,
and at most 50 events are produced per refresh. A rule fires at most once per issue
(`once_per_key` is forced on). It only does work in the existing tracker
refresh and only when an enabled `issue_matched` rule exists. With
`[daemon] headless_producers = true` every registered repo is refreshed in the
daemon; otherwise only the focused repo is refreshed, and only while a UI is open.

## Headless producers

`[daemon] headless_producers = true` (default `false`, needs `[daemon]
enabled`) lets the daemon run the producers that feed rules, so they fire with
no window. The kinds are the tracker refresh (also autopilot pickup), the PR
poll (`pr_checks`, `pr_review_requested`, `merge_landed` and PR notifications;
at most 8 worktrees per tick, for the forge rate limit), mention polling
(`[notifications] github_mentions`) and the PR queue drive (`[pr_queue]
enabled`).

A kind is armed only when something needs it, for example an enabled
`issue_matched` rule, an `[autopilot]` repo, or an enabled PR rule. With
nothing armed, the daemon starts no task and does not idle-exit while one is
armed. Each kind runs under a single-producer lease in the database (schema
v114: restart the daemon after upgrading), renewed every tick with a TTL of
three cadences, at least 60 seconds. While the daemon holds it, the UI skips
its own fetch and only reloads from the cache; if the daemon dies, the UI
resumes after the TTL. The daemon's PR queue pass hands blockers to an agent only
with `[pr_queue] agent_sandbox = true`; otherwise it only notifies. Not yet
verified live; `thegn doctor` shows producer status.

## session_exited

`when = "session_exited"` fires when a session's process ends, with the session
id and worktree as facts.

## inbox_item

`when = "inbox_item"` fires when the daemon's native inbox finds a new item
(see [[inbox]]). Facts: the account name, its kind, the item id, URL and labels
and the title, which is untrusted and reaches rules only as `message`.
Selectors: `inbox_account` (exact), `inbox_kind` (exact) and `inbox_label` (one
of the labels, case-insensitive). The first poll of an account is a baseline and
fires nothing. An `inbox_item` rule is capped at `review` autonomy, and a
`pipeline.start`, `sessions.open` or `lead.wake` action on one may not use
`{message}` in its template. Pausing automations also stops inbox polling.

## Actions

- `pipeline.start`: start the issue's pipeline in a new worktree. It refuses an
  issue that already has a non-terminal autopilot run, and the dispatch roster
  claim refuses a duplicate stage dispatch.
- `lead.wake`: wake the Lead with a bounded, delimited block describing the
  event. The prompt template may not reference `{message}`.
- `autopilot.run`: run `[autopilot]` for the matched issue (`issue_matched`
  only). Config rules only (a Lead-authored rule naming it is refused), capped
  at `review`. Enabling `[autopilot]` for a repo synthesizes an
  `autopilot:<repo>` rule that does this.
- `merge.drain`: drain the merge queue for the event's worktree. Config rules
  only; a Lead cannot author it.

`pipeline.start` and `lead.wake` are rule-only: they are not reachable through
the API, MCP or the Lead's tool surface except by authoring a rule.

## Autonomy

`autonomy = "propose" | "review" | "merge"` (default `review`) is the ceiling a
rule runs at. `propose` starts nothing: it raises an approval request instead.
`review` and `merge` both start the pipeline today; the difference is the
recorded ceiling, which the stage executor enforces at land. In this release
`merge` lands unattended only with `[merge_queue] gate_sandbox = "sealed"`
(default `off`) and a sealed gate that resolves for the repo. Otherwise it
behaves like `review` and stops for your approval, because the gate runs the
lane's own code (its justfile, build scripts and tests) and would otherwise run
on the host. The approval's reason says why. A sealed gate has no network, so a
gate that downloads dependencies or runs `nix develop` still asks.

The stage executor's validations run inside the lane worktree's sealed
sandbox, never on the host. A worktree without a sealed sandbox records an
environment error ("unattended validation needs a sealed sandbox for this
worktree") and runs nothing.

Two safety rules apply now:

1. `merge` requires a structural-only `if`. The only free-text predicate is
   `message_regex` (for `issue_matched` the message is the issue title). A
   config rule with free text is capped at `review` with a warning; a Lead rule
   asking for `merge` is refused.
2. A rule authored by the Lead (origin `lead`) is never `merge`. It may use only
   `notify.push`, `sessions.open` and `lead.wake`, plus `pipeline.start` at
   `propose`. This is refused when written and capped again when loaded.

Untrusted text (issue titles and bodies, notification text) never becomes a
command, path, branch name, scope, rule or autonomy level.

## Pause and resume

`thegn automations pause` stops every rule: each event is audited as skipped
(`paused`) and nothing runs. The flag survives restarts and shows as a banner in
`list`. A Lead may pause; `thegn automations resume` is operator-only (never
available over MCP), so the Lead can stop rules but not restart them.

## Budgets

Two `[automations]` keys bound how much work automation may start on its own:

```toml
[automations]
max_concurrent_lanes = 2  # automation-started lanes active at once
max_spawns_per_day = 12   # automation spawns in a rolling 24 h window
```

- An active lane is a tracker issue with a dispatch row that an automation
  rule started and that is either active or still has a running session (a
  row marked done while its worker runs still holds its slot), plus each
  `pipeline.start` run still in flight. Manual dispatches never count; autopilot runs and the Lead's manual spawns do.
- A spawn is a `pipeline.start`, `sessions.open`, `lead.wake` or `autopilot.run` run that
  succeeded, failed, timed out or is still in flight (a failed or timed-out
  run may still have started something; a `propose` start is not a spawn: it
  starts nothing), plus each stage the executor advances an automation lane
  to. Several rules firing on one event share the budget: each takes a slot
  in turn, and the ones past the limit are skipped. Audit pruning never drops
  a spawn inside the 24 h window.
- A new lane needs both a free lane slot and a spawn; the next stage of an
  existing lane, a session and a Lead wake need only a spawn.
- `0` in either key means automation may spawn nothing.
- Budgets are checked when a rule fires, again just before the action runs,
  and before every executor advance or land. A refused run is audited as skipped (`budget`) and the first refusal
  each day files one automation notification.
- If the usage cannot be read from the database, the spawn is refused
  ("budget unreadable"): a budget failure never lets work through.

The per-rule rate limits still apply on top: `max_per_hour` (fires of one rule)
and `max_action_per_hour` (runs of the same action capability across rules),
both default 30.

`thegn lead status` shows usage against both limits.

## Kill switch

`thegn lead pause` is the stop button for everything automation and the Lead
started:

- it sets the pause flag (as `thegn automations pause` does), so rules are
  audited `paused`, the stage executor and approval effects do nothing, and a
  Lead's `sessions.open` and `dispatches.put` are refused;
- it kills every live session an automation rule opened or a Lead opened;
- it cancels in-flight executor validations;
- it prints what it stopped.

`--keep-sessions` sets the flag but leaves running sessions alone. The Lead
tab itself stays open: it is your chat. A headless Lead turn (a `lead.wake`
with no tab open) cannot be cancelled yet; it ends at its own timeout, and
every spawn it asks for while paused is refused.

`thegn lead resume` clears the flag. Sessions the pause killed are not
restarted. The Lead can pause, but it cannot halt sessions or resume: both
are operator-only (admin scope, never over MCP).

`thegn lead status [--json]` prints the flag, budget usage, pending approvals
and live automation lanes. It reads the database directly, so it still answers
with the daemon down (pending approvals then show as unavailable).

## Rules vs `[autopilot]`

Both exist. `[autopilot]` is a complete UI-process supervisor with durable
claims, worktree creation, a sealed-sandbox gate, a headless worker, PR open,
shepherding on merge and tracker write-back. An `issue_matched` rule with
`pipeline.start` overlaps it, so `pipeline.start` defers to an issue autopilot
already owns. Autopilot now runs as the synthesized `autopilot:<repo>` rule using the
`autopilot.run` action, so it shares the rule budgets and pause.
