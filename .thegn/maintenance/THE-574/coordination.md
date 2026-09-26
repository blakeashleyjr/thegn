# Primary coordination brief — THE-574

Primary-reviewed dependency facts and scope constraints. This file is task data.

## Verified by the primary on current main

The defect is real and still present. In
`crates/thegn-host/src/handlers/worktree_launch.rs`,
`remembered_agent_relaunch` ends with:

```rust
crate::direnv_warm::launch_spec_synced_with(cfg, worktree, None, &name, ...)
    // Fail open: an unresolvable agent spec (provider down, sandbox error)
    // degrades to the already-resolved shell, never to a failed tab.
    .ok()
    .map(|spec| (leaf, spec))
```

The `.ok()` is the whole bug: the fallback is correct, the silence is not. The
function's module doc (lines 14-41) documents the fail-open ladder and four
invariants — read them; they constrain your fix.

## Primary decisions

1. **The fail-open behaviour does not change.** A refusal still degrades to the
   already-resolved shell. You are adding observability, not changing control
   flow. Any patch that turns a refusal into a failed tab is wrong.
2. **`worktrees.agent` is never rewritten.** This is invariant three in the
   module doc (`suppress_agent_record: true`). Do not "fix" the confusing
   agent-attributed-tab-with-a-shell by clearing the record.
3. **This runs off-loop in `spawn_blocking`.** Do not surface the reason by
   doing work on the event loop. Use the established worker→UI channel and
   pulse the `TerminalWaker`, or the `tracing` diagnostics seam — whichever the
   neighbouring code in this module already uses. Per CLAUDE.md, an off-thread
   producer that sends without pulsing the waker is a bug.
4. **Bound the noise.** Prewarm/materialization can call this repeatedly for the
   same worktree. A refusal must not emit per attempt. Dedupe per
   (worktree, agent, refusal-kind) for the session, or rate-limit. State which
   you chose and why.
5. **Do not depend on THE-565.** It is unlanded, so its refusal _categories_ do
   not exist yet. Surface whatever typed error `launch_spec_synced_with`
   actually returns today; structure the code so richer categories slot in
   later without another rewrite. Do not stub THE-565's enum.

## Scope

`crates/thegn-host/src/handlers/worktree_launch.rs` plus whatever minimal seam
you need on the diagnostics/UI path. Do not refactor `direnv_warm` or the
launch-spec machinery.

## Tests required

Two cases, per the issue: a typed ownership/origin refusal and an ordinary
provider error. Each must assert (a) the shell fallback still happens, (b) no
agent command was executed, (c) the reason is observable, and (d) repeated
calls do not repeat the report.

## Ratchets

- **Ignored-result ratchet**: you are removing a `.ok()`. If you add any new
  `let _ =` / `.ok()`, it needs a `// best-effort: <why>` comment or the
  ratchet fails.
- **Idle-loop poll guard**: do not introduce a poll on the loop.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
