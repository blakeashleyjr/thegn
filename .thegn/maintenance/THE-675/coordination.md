# Coordination brief — THE-675 (primary)

## Scope

The `util::git_out` chokepoint and whatever private module owns the reused
process. **Do not** convert the 77 call sites; the whole point of the chokepoint
is that callers do not change. If your plan touches call sites, that is a finding
to report, not work to do.

Explicitly out of scope: `GitLoc::git_command`, `util::git_cmd`, `util::git_ok`.
One helper, this change.

## The decision the primary most wants from your plan

`git cat-file --batch` serves **object reads** — it cannot answer
`rev-parse`, `status`, `rev-list` or `for-each-ref`. The measured 1.9 ms floor in
the issue is from `git rev-parse HEAD`, which a batch process **does not help**.

So before designing anything, **partition the 77 `git_out` sites**: which are
object reads that a `cat-file --batch` can actually serve, and which are not.
Report that count. If the answer is "few", the honest conclusion is that this
change is not worth its complexity, and **saying so is a successful outcome for
this row.** Do not design a process pool to serve three call sites.

A plausible alternative worth pricing in the same plan: a persistent
`cat-file --batch-check` for existence/type/size probes, which is often the
hotter pattern than reading contents.

## Hard constraints (these are what make this risky)

- **0% idle is a contract, not a goal.** A parked child must generate **zero**
  wakeups. It sits on a blocking read in its own thread, or it is not acceptable.
  A design that polls the child, or that adds a timer to reap it, fails.
- **Never on the event loop, never before the first frame.** These calls are
  already off-loop; keep them there.
- **A stalled child must never block a caller indefinitely.** State the deadline
  and what happens when it expires — and the fallback must be "spawn a
  one-shot git as before", so a wedged batch process degrades to today's
  behaviour rather than failing the operation.
- **Lifecycle ownership.** A long-lived child per repo needs an explicit owner, a
  deterministic teardown, and no descendant left behind on exit. Note that
  THE-171 is concurrently reworking per-child lifecycle ownership for PTYs — do
  **not** touch that code, and do not build on an API it is changing. If you find
  you need one, report it.
- Any new long-lived thread must declare a **QoS class** (`platform::qos`);
  the default is `Interactive`, which is wrong for a helper. `Utility` or
  `Background` as appropriate.
- The batch protocol is line- and length-delimited over a pipe. **Partial reads
  and a child that dies mid-response are the two cases to get right**, and both
  need tests that do not rely on timing.

## Deliverable

A plan the primary reviews and greenlights before implementation — including the
site partition above. No production edits in this stage. A recommendation **not**
to proceed, with the counts behind it, is an acceptable and useful plan.
