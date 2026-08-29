# Harden pipeline slot accounting against stale-row re-dispatch

Incident: 2026-08-29 (no Linear issue — operational)

## Why

On 2026-08-29 the agent pipeline accumulated **121 dispatch rows stuck
`running`**, ran **33 issues concurrently against a configured budget of 9**,
and left **386 GiB of `target/` across 33 worktrees**, taking the filesystem to
90% full. No worker misbehaved: 119 of the 121 rows had a real, git-tracked
handoff artifact. The pipeline did the work and then could not admit it.

Four independent defects lined up. Each is individually plausible; together
they form a runaway.

1. **The handoff contract was unenforceable.** `311d7e60` (THE-88,
   2026-08-28 19:33) made a worker *report* mandatory for `set-status done`.
   The deployed `[[pipeline.stages]]` prompts were never updated to ask for
   one — they never even referenced `{row}`, so a worker could not have filed
   a report if it wanted to. Every row dispatched after that commit became
   unclosable without `--force`. Nothing checked this: the org chart was
   valid, the placeholders were valid, and the failure only showed up as a
   slowly growing roster.

2. **`running` meant two opposite things.** A row's status could not
   distinguish a live worker from one that had exited hours ago into a row
   nobody closed. The supervisor filled slots by counting *live daemon
   sessions*, so every exited-but-open row read as free capacity — and it
   re-dispatched into it, repeatedly.

3. **Slot accounting was not atomic.** `dispatch list` → judgment →
   `dispatch put` is a read-modify-write with no lock, so two monitors (or one
   monitor and its own restart) both see a free stage and both insert.

4. **An older runtime silently drove a newer database.** A v57 build operated
   a v62 database for hours. The mismatch was tolerated with a warning
   (326,912 of them from one process, before `4eba4407` added a `Once`), and
   `thegn --version` could not tell the two builds apart because the crate
   version had not moved.

Note what is **not** on this list: duplicate dispatch of identical work. All
121 rows had distinct artifacts — the "eight dispatches into one worktree" was
a legitimate progression (3 parallel chunks → 4 revision cycles → 1 security
fix). Any fix keyed on issue+stage+worktree alone would have refused real
parallel work. Identity must include the artifact.

## What changes

Structure-not-judgment still holds: thegn does not gain a scheduler, does not
advance `next`, and does not decide what to dispatch. What it gains is
**arithmetic the supervisor could not do correctly from outside**, and the
ability to *say* what state a row is in.

- **Refuse an older build against a newer database** (`db::schema_refusal`),
  with one actionable error instead of a repeated warning. Escape hatch:
  `THEGN_ALLOW_SCHEMA_DOWNGRADE=1`.
- **`thegn doctor` prints the schema pair and both binaries** — the CLI's own
  path and each registered daemon's actual `/proc/<pid>/exe`. These are the
  facts `--version` could not show.
- **Schema v63**: `agent_dispatches.exit_code` / `.exited_at_ms`, stamped on
  pane exit *even when the status deliberately does not move*, plus
  `pipeline_leases`. `pipeline_run::row_liveness` derives
  `Live | ExitedUnverified | Closed`; `dispatch list` prints `running!exited`
  and states plainly that such rows are not free capacity.
- **`thegn dispatch claim`** — the atomic alternative to `list` + `put`. Runs
  a pure policy (`pipeline_claim::decide`) inside `BEGIN IMMEDIATE`, so the
  check and the insert cannot be split. Refuses a duplicate (keyed on
  issue + stage + worktree + **artifact**, so parallel chunks stay legal) and
  refuses at stage capacity, counting exited-but-unclosed rows as occupied.
  `--allow-duplicate <reason>` is the auditable override and commits its
  justification as a note in the same transaction.
- **`thegn dispatch lease`** — monitor ownership with an expiry, so two Leads
  cannot drive one pipeline and a crashed holder's claim lapses unaided.
- **`validate_stage_contracts`** — a stage prompt that never names `{row}` or
  never asks for `thegn dispatch report` is a config error, surfaced both at
  `config validate` and at load. This is the check that would have caught the
  incident's config before it ran.
- **Disk-reclaim hysteresis** — a 6h per-worktree cooldown and a 5-point
  overshoot past the warn line, so reclaim and rebuild cannot oscillate; plus
  an exemption for worktrees whose pipeline work is still unverified.

## Impact

- `tasks.md`: the agent-pipeline group (THE-57 / THE-76 / THE-86 / THE-88
  lineage). This change repays reliability debt in that surface rather than
  adding a feature.
- Schema v62 → v63, purely additive; a pre-v63 row reads `None` for the exit
  pair, which means *unknown*, never *exited*.
- Behavioural change with a blast radius worth naming: a build older than the
  on-disk schema now **fails to start** instead of running read-only. That is
  the point — but it means a stale checkout sharing the state dir will stop
  working until it is rebuilt.
