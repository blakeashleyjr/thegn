# Primary review + greenlight — THE-601

Reviewing row 624. **APPROVED. Four decisions below, then the ordered steps
restated in full.**

A previous dispatch of this row correctly refused to proceed because this
document said "steps 2–7 stand as written" while being the only artifact handed
to it — `--parent-artifact` passes one path, so the investigation plan was not
readable from the coding session. That was the primary's error, not yours. The
steps are inlined below; this document is now self-contained.

Your evidence is right, including the distinction that matters:
**THE-589 caps only later retained diagnostics, not this initial allocation.**
Conflating those would have produced a fix that changed nothing. You also noted
THE-210's workspace safety is landed and must be preserved — it is, and it is
recent, so do not refactor around it.

## DECISION 1 — timeout keys, names and defaults

Put them under `[merge_queue]`, beside the existing gate keys (`gate_command`,
`gate_reuse_worktree`, `gate_target_dir`) — the gate's configuration already lives
there and a second home would be worse than a long name:

- `gate_timeout_secs`
- `gate_setup_timeout_secs`

**Default both to `0`, meaning disabled**, and document that plainly. Reason: the
configured gate on this machine is a full workspace test run, which legitimately
takes tens of minutes, and a default deadline short enough to be useful would
fail good branches. An operator who wants a deadline sets one. A silent default
that turns a slow build into a red gate would be a false accusation, which is the
failure mode this whole area is careful about.

Note `0`-as-disabled has a precedent trap in this repo: `merged_ttl_secs = 0`
means _never sweep_, which silently made a smoke helper vacuous. So **document
`0` at the key and make the disabled case explicit in a test**, rather than
leaving it to be inferred.

Because these are new `section.key` pairs, they trip **three** ratchets — config
example coverage, env overlay, strict config — exactly as your step 3 says. Good
that you already listed them.

## DECISION 2 — a caller cancellation token is OUT of scope

No current caller needs one. The stated failure is a gate that never completes
because a grandchild holds a pipe; a deadline plus process-group teardown and
pipe settlement covers that entirely. A cancellation token would thread a new
seam through fold, land and the queue for no present benefit. If you find a
concrete caller that cannot be served without one, report it as a follow-up
finding rather than adding it here.

## DECISION 3 — an escaped descendant holding a pipe: bound, then poison the lease

You asked what to do when an uncontained descendant retains a pipe or is stuck
in uninterruptible I/O, since joining can block forever but detaching or reusing
the lease violates acceptance. Neither horn is necessary:

1. **Never block indefinitely.** When the gate deadline expires, stop reading,
   close our read ends and record `output truncated: a descendant retained the
pipe` in the captured tail. A reader that cannot be joined is abandoned only
   after its pipe ends are closed on our side, and the abandonment is recorded.
2. **Do not release the lease — poison it.** Mark the reusable gate worktree
   unusable and carry the reason; the next gate allocates a fresh path rather
   than reusing or deleting a directory that may still have a live writer in it.
   That satisfies "no lease reuse while old work remains" without waiting on a
   process the OS will not schedule.
3. **`GateVerdict::Error`, always.** This is an infrastructure hold. It must not
   reach `classify_exit` and must not enter `bisect_offender`.

A poisoned lease must be **visible**, not silent: surface it where
`thegn doctor` can report it, because a gate that quietly stops reusing its
warm worktree turns every later fold into a cold compile and nobody would know
why.

## DECISION 4 — Windows Job Object setup failure must fail closed, before spawn

You found that grouped spawn degrades to direct-child-only ownership when Job
Object setup fails. **Remove that degradation on this path.** If containment
cannot be established, do not spawn: report `GateVerdict::Error` naming the
setup failure.

Ownership is the entire acceptance criterion here, and a gate that runs without
it is exactly the configuration that leaks. Failing closed costs an operator a
clear error; degrading costs them an unkillable build they cannot attribute. Keep
the degradation for any other caller that relies on it today — change the gate's
spawn, not the platform default.

## The ordered steps, restated (this is the work)

1. **(Done — this document.)** Timeout key names, defaults and ranges are fixed
   in Decision 1; cancellation-token scope in Decision 2; escaped-descendant and
   Windows containment policy in Decisions 3 and 4.
2. Add the **smallest shared subprocess-capture primitive** used by both
   `Workspace` and `NativeWorkspace`: independent fixed-capacity byte tails for
   stdout and stderr, incremental reads, the process owner retained through
   termination and direct-child reap, explicit pipe settlement, and a typed
   success / red / infrastructure / timeout result. **Preserve the configured
   command's exact argv and shell behaviour.**
3. Add the setup and gate deadlines from Decision 1 — through defaults,
   overlays, apply, `config/config.toml.example`, the schema/example/env-overlay/
   strict-config ratchets and focused config tests. **No silent deadline.**
4. Wire `integrate_gate.rs` to hold workspace identity and the lease until both
   process and pipe settlement; map setup timeout/failure and gate
   timeout/transport/reap errors to `GateVerdict::Error`; only a completed gate
   exit code reaches `classify_exit`. **Unknown child ownership prevents cleanup
   and reuse** (Decision 3's poisoning is how).
5. Focused private regressions in `integrate_gate_tests.rs`: large dual streams,
   a UTF-8 sequence split across a read boundary, an endless writer, pipes held
   after the leader exits, ignored termination requiring force kill, timeout,
   spawn/reaper/reader failure, ordinary pass, ordinary red, no unrelated
   signalling, and no early lease reuse. Check that public fold/land/queue
   results keep their hold/no-advance behaviour on infrastructure failure.
6. Format, then the narrow config/host tests, then the host check. See
   **Validation** below for what the sandbox actually permits. A native fixture's
   own command-level timeout does **not** prove production cancellation — say so
   rather than letting it stand in.
7. The primary reviews the final diff and test evidence and greenlights landing
   separately. This document is not that approval.

Scope limits carried from your plan and confirmed: touch
`platform/{mod,unix,windows}.rs` only if the runner provably needs a stronger
owned-group API — do not widen generic process APIs speculatively — and touch
`openspec/specs/merge-queue/spec.md` only where its gate contract is genuinely
silent about output bounds, deadlines or infrastructure holds.

## Confirmed, and the line that must not move

**Exit classification is sacred.** Setup failure, timeout, transport failure and
reap failure are `GateVerdict::Error` — infrastructure holds. Only a completed
gate exit code may reach `classify_exit`. A timeout must never present as a red
gate, and unknown child ownership must prevent cleanup and reuse rather than
assuming the lease is free.

Also: this crate **bans blocking child waits** outside sanctioned off-loop sites.
Read `crates/thegn-host/clippy.toml` before adding a subprocess call and follow
the existing `#[expect(clippy::disallowed_methods)]`-with-a-reason pattern. And be
honest in the comments about what cannot be cancelled — a process blocked in
uninterruptible I/O will not die on TERM, and the code should say so rather than
imply a guarantee.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in the previous chain shipped something that did not
build and the primary caught each — that division of labour is expected, an
optimistic report is not.
