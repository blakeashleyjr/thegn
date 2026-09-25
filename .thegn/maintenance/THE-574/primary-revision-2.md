# Primary revision brief — THE-574 (round 3)

Row 554 filed five findings. The primary **compiled the branch** and confirms
the most important one.

## F1 — CONFIRMED BY THE PRIMARY. The branch does not compile. Fix this first.

Round 2 was reported `implementation-ready`, but
`cargo check -p thegn-host --all-targets` fails. Do not proceed to anything else
until these are green. The exact errors:

1. **`worktree_launch.rs:305`** — `apply_relaunch` declares `-> RelaunchOutcome`
   but its body has no tail expression, so it returns `()`.
   rustc: _"remove this semicolon to return this value"_ at line 314. You added
   the return type and the delegated call but left the `;`, so the outcome F2
   complains about being discarded is literally thrown away here.

2. **`worktree_launch.rs:757` and `:760`** — `let refusal = || { … }` takes 0
   arguments, but `apply_relaunch_with` requires
   `impl FnOnce(&Config, &str, u32) -> Result<Option<(u32, LaunchSpec)>, RelaunchRefusal>`
   (the bound is at `:324`). Two call sites pass the 0-arg closure.

3. **`worktree_launch.rs:766`, `:770`, `:826`** — `provision::SpecError` does not
   implement `Debug`, and these sites `unwrap()`/`expect()` a `Result` carrying
   it. Either derive `Debug` on `SpecError` at
   `crates/thegn-host/src/handlers/provision.rs:45` (preferred — an error type
   should be `Debug`) or match instead of unwrapping.

Fix the root cause, not the symptom: the outcome must actually be **returned**
and **used**, which is also F2.

## F2 — ACCEPTED (and it is the same defect as F1.1)

`RelaunchOutcome` is declared, then discarded by `materialize`/`prewarm` and
absent from `SpecBatch` bookkeeping. Return it from `apply_relaunch`, thread it
into the batch's bookkeeping, and make the refusal visible there — per the
round-2 brief, **without** changing fail-open behaviour or prewarm retry
scheduling. If threading it genuinely forces a scheduling change, STOP and
report rather than making one.

## F3 — ACCEPTED

`bounded_reason` fully materializes and redacts the unbounded error text
_before_ truncating. Truncate **first**, then redact the bounded slice. A
hostile or pathological error string should never be fully materialized and
scanned. Cap the input you look at, not just the output you emit.

## F4 — ACCEPTED, narrowly

The 256-entry FIFO caps the entry _count_ but each entry still retains
caller-sized worktree/agent strings. Bound the stored key itself: store a hash
or a truncated key rather than the full strings. Exact identity is not needed —
this is a de-duplication set, and a bounded key with an astronomically small
collision chance is fine for suppressing a repeated diagnostic. Note the
trade-off in a comment.

## F5 — ACCEPTED

Tests inspect argv but do not prove no process was spawned. Add a real
spawn sentinel (a counter or a temp-file marker the fake launcher touches) and
assert it is never triggered on the refusal path. "We looked at the argv" is not
the same claim as "nothing ran".

## Process note for this round

Round 2 claimed `implementation-ready` for code that does not build. The stage
contract forbids you from running cargo, and that is fine — but it makes the
**claim** the thing to be careful about. Report what you actually verified and
nothing more. `rustfmt` passing is not evidence that the code compiles.

## Unchanged constraints

Fail-open preserved; `suppress_agent_record: true`; `worktrees.agent` never
rewritten; WARN level; no work on the event loop. The primary runs the gate.
