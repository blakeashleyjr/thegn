# Primary review + greenlight — THE-210

Reviewing row 567's investigation
(`.thegn/pipeline/THE-210/maintenance-investigate/567.md`).

**Verdict: APPROVED to implement**, points 1-5 as written. The three review
questions are answered below. Also: **you may run `cargo check -p thegn-host
--all-targets` and focused `cargo test`** — the coordination brief authorized it
and this round should use it, because point 1 changes a platform seam and that
is exactly where the previous batch kept shipping non-compiling code.

The plan is well-shaped and its scoping instinct is right: change the
admission/identity decision, not the workspace lifecycle. The finding that
THE-597 already fails closed before shared-worktree mutation is accepted — say
so explicitly in the implementation artifact and do not re-fix it.

## Q1 — Windows: take the narrowly scoped unique-path route

Do **not** build full Windows verified file/directory admission in this lane.
That is a real platform-security workstream and it would swallow the fix.

Approved: isolate the Windows branch **before** Unix-only reused-state
admission, and have it use a native unique worktree path. The invariant that
must hold is the one in the issue — Windows never silently selects the shared
reused path without an exclusion mechanism. Losing the warm target dir on
Windows is an acceptable cost; a misattributed verdict is not.

Record the missing Windows verified-admission seam as an explicit follow-up
finding, with the file it would live in.

## Q2 — canonical identity: the Git common directory, resolved

Use the **verified Git common directory**, canonically resolved, as the
authority. Rationale: it is precisely the property you want — every linked
worktree of one repository shares one common dir, and two unrelated repositories
never do. A filesystem identity (device + inode) of the _caller's_ path gives
the opposite answer for linked worktrees, which is the bug being fixed.

Pair it with filesystem identity of that resolved common dir as a
**confirmation**, not as the key, so a moved or recreated repo cannot alias an
old lock. Do not hash a raw caller path. Do not keep `DefaultHasher` for the
key: it is not stable across Rust releases, so a toolchain bump would silently
orphan every existing lock. Use a stable digest.

## Q3 — `gate_target_dir` must NOT be shared by an isolated gate

Your instinct in point 2 is right; making it explicit so there is no ambiguity:

- Reused path selected → reused target dir, as today.
- Isolation fallback → **never** the reused/default target dir. Cargo takes an
  exclusive flock on `target/<profile>/.cargo-lock` for the whole compile
  (CLAUDE.md documents this as the reason `shared_target_dir` is not the
  default), so two isolated gates sharing one target dir would serialize
  precisely when we fell back to isolation _because_ of a concurrency problem.
- An explicitly configured `gate_target_dir` is only safe for the isolated path
  if the operator opted into it knowing it serializes. Default to a private
  target dir under the unique workspace and say so in the diagnostic.

## Point 5 — say more than the current headline

Strongly endorsed, and it is why this lane is in the batch. The primary lost
several runs to `was NOT gated — gate preparation or identity unavailable`
with no cause, no path, and no log (`land` discards the `GateVerdict::Error`
log entirely). Your proposed messages are the right shape. Make sure the reason
survives to `land`'s output, not just into the dropped log — if `land` needs a
small change to print it, that is in scope.

## Tests

The ordered plan is approved. The deterministic two-process/barrier concurrency
test is the important one — assert each verdict observes **its own** requested
OID. Add the acquisition-failure test proving the shared `wt` is untouched
(assert its mtime/HEAD is unchanged, not merely that an error was returned).

## Scope

`integrate_gate.rs`, `integrate.rs`, the `platform/` gate seam, and tests. No
configuration or queue-architecture change. Platform `#[cfg]` belongs behind
`platform/`, or needs a reasoned shrink-only entry in
`test/platform-cfg-host-ratchet.txt`.
