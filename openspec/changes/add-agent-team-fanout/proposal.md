# Add agent-team fan-out into sandboxed worktrees

## Summary

A single verb — `szhost team <task>` — fans one task out to **N agents, each in
its own isolated git worktree + sandbox**, launched as **visible panes**, with
the caller's pane kept as the orchestrator. This is the multi-agent workflow
cmux ("Claude Code Teams") and limux (`agent-team`) demonstrate — spawn agents as
visible panes, never hidden background processes — but grounded on superzej's
isolation substrate, which they lack: each teammate runs on its own branch in its
own worktree, optionally sandboxed, so their edits never collide. An optional
`--best-of-N` mode runs the _same_ task in N isolated attempts and surfaces them
in the existing diff/review pane to pick a winner.

## Impact

- **Q 224** — batch/parallel launch: the one-verb fan-out that spawns a task
  across multiple worktrees/agents at once.
- **Q 225** — best-of-N attempts (currently deferred): `--best-of-N` runs the
  same prompt in N isolated worktrees; reviewer picks/merges one.
- **S 244** — abtop-style fleet view: the spawned team is the first real consumer
  of a fleet/roster view (each teammate a row with its activity + attention).
- **T 259/260/263/267** — review & merge: teammate results flow into the existing
  diff review pane, one-key needs-attention jump, approve→merge / reject→discard,
  and cycle-through-diffs.

Extends the `agent` capability; reuses the `sandbox` capability, worktree
creation (D 41–43), and the warm sandbox pool. **No new DB schema** beyond the
existing worktree/session tables (a team is a set of worktrees + a grouping tag).

## Rationale

The hard part of multi-agent work is not spawning agents — it is keeping their
changes from stepping on each other and knowing which one to keep. cmux/limux
spawn agents into a _shared checkout_ (worktree isolation is an open request in
cmux, absent in limux), so parallel agents on the same repo conflict. superzej
already ships **worktree-per-tab + podman/docker/bwrap sandboxing + a warm
sandbox pool + a diff/review pane**. Composing those into a one-verb fan-out is
mostly wiring existing primitives, and it yields a _strictly stronger_ version of
the neighbors' headline feature: "run N agents on the same task, each sandboxed
in its own worktree, then compare and merge the best." The "visible panes, not
hidden processes" posture is the mental model to keep.

## Non-goals

- **A general task queue / scheduler (Q 215, 226–228)** — team fan-out launches a
  bounded set now; queueing/priority/dependencies are separate roadmap items.
- **Automated winner selection** — `--best-of-N` _surfaces_ the attempts for a
  human (or a later scoring pass) to choose; it does not auto-pick.
- **A bespoke A2A wire protocol** — coordination is the orchestrator pane driving
  teammates (and reading their panes); a structured peer protocol is out of scope
  (limux's TTY-injection A2A is explicitly not adopted).
- **Any AI hard-dependency in the shell** — the verb belongs to the agent layer;
  the worktree/sandbox fan-out primitive it builds on stays AI-free.
