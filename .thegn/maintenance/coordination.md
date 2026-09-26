# Coordination brief — THE-690

Fifth and current blocker in the merged-worktree sweep chain. Four causes are
already fixed and landed — THE-685 (`0a3e3150`), THE-686 (`2acdf364`), THE-687 and
THE-688 (both `e1a64f61`) — and the sweep **still collects nothing**: all thirteen
candidates now fail on this one guard. The repo owner hits this every day.

## What the primary already measured — build on it, do not redo it

`crates/thegn-host/src/merge_cleanup.rs:108-131`, `oci_resources_absent`, refuses
when **any** OCI binary exists **anywhere on PATH**. It never asks whether a
container references the worktree. Measured on this machine:

- `docker` → `/run/current-system/sw/bin/docker`, `podman` →
  `/run/current-system/sw/bin/podman`. Both live in the NixOS system profile, so
  they are on PATH permanently and unconditionally.
- Two containers exist, `cms-garage-1` and `cms-postgres-1`, belonging to an
  unrelated project.
- Inspecting every container's mounts finds **no reference to any kept worktree**.

So the refusal is not about these worktrees at all. It fires on the mere presence
of a tool, which makes the sweep permanently inert on essentially every
development machine.

Re-verify the citation on your branch and report it confirmed (or moved); the
primary does not expect it to have changed.

## This is the third instance of one pattern — name it in your report

- THE-687: a **cache field** (`result_oid`) absent ⇒ permanent refusal.
- THE-688: a **saved tab layout** row ⇒ permanent refusal.
- THE-690: an **installed binary** ⇒ permanent refusal.

Each substituted a cheap proxy for the question actually being asked. Yours is
the broadest, because it does not depend on anything about the worktree. If you
notice a fourth instance of the same shape while you are in here, **report it as a
note** — do not fix it.

## What to implement

Ask the real question: does any container, running or stopped, have a mount whose
source is at or under this worktree path?

1. **No OCI runtime discoverable** ⇒ `Ok`, exactly as today.
2. **Discoverable** ⇒ query it and refuse **only** when a container actually
   references the path. Prefer one bounded single-shot query per runtime (e.g.
   `ps -a` plus an `inspect` format over mount sources) rather than a query per
   container if you can get it in one call.
3. **Discoverable but unqueryable** — daemon down, permission denied, timeout ⇒
   refuse conservatively, with a reason that says it could not be **queried**.
   That is a legitimate unknown. "The binary exists" is not, and that distinction
   is the whole issue.

Match at or **under** the path: a bind mount of a subdirectory still means a
container holds part of that worktree. Compare canonicalised paths — a symlinked
or relative mount source must not slip past — and treat a mount source you cannot
canonicalise as a match, not a miss.

## Hard constraints

- **The query must be bounded** — an explicit timeout and an output cap. A hung
  container daemon must not hang a sweep; it must become the
  "could-not-be-queried" refusal.
- **It must never run on the event loop.** The crate bans blocking child waits
  outside sanctioned off-loop sites; `clippy.toml` in `crates/thegn-host/`
  explains the rule and the `#[expect(clippy::disallowed_methods)]` convention
  with a one-line justification. Read that file before you add a subprocess call,
  and follow the existing pattern rather than inventing one.
- **Fail closed on every genuine unknown.** This guard authorises deleting a
  directory; when in doubt it must refuse. What must stop is refusing when there
  is no doubt at all.
- `--force` bypasses the TTL clock and nothing else.

## Out of scope

- The other four landed fixes: THE-685's filter drivers, THE-686's
  `StatusObservation` / `Refusal::Changed`, THE-687's `derive_landed_commit` and
  landed-OID guards, THE-688's layout teardown. Preserve them; do not refactor.
- **THE-689** — the accepted session-admission-lock and ghost-tab races. Filed,
  deliberately deferred, not yours.
- The `has_cleanup_tenancy` / `has_cleanup_dispatch` predicates, the
  submodule/special-index guards, TTL arithmetic, branch-retention holds.

## `test/smoke.sh` — read this, it is load-bearing

Smoke currently **works around** this bug: it builds a minimal PATH containing
only `git` and `sh` so the guard cannot fire, with the comment _"Do not let … OCI
tools installed on the developer's machine imply unresolved runtime custody."_
That comment describes the defect precisely.

Once the guard is correct, that workaround should no longer be needed to exercise
a real sweep. Either remove it, or keep it deliberately and say in a comment why.
Do not leave it there unexamined — and check whether the surrounding sweep cases
still assert what their names claim.

This matters because smoke has already caught two things in this chain that
clippy and 9100+ unit tests did not: THE-686's inverted `"sweep --force preserves
ignored work"` case, and it is the only place the real end-to-end sweep runs.

## Acceptance criteria (from the issue)

- [ ] A merged worktree with no container referencing it is swept once its TTL has
      elapsed, **on a machine with docker and/or podman installed.**
- [ ] A worktree bind-mounted by a container (running **or** stopped) is never
      swept, with or without `--force`.
- [ ] A discoverable-but-unqueryable runtime still refuses, naming that it could
      not be queried.
- [ ] No OCI runtime present behaves exactly as today.
- [ ] The query is bounded (timeout and output cap) and runs off the event loop.
- [ ] Tests cover: no runtime; runtime with an unrelated container; runtime with a
      container mounting the worktree; runtime present but unqueryable — **each
      with and without `--force`.**
- [ ] The smoke workaround is removed or deliberately retained with a reason.

The tests are a matrix, not four cases. Do not report the row finished with the
`--force` half missing.

## Testing traps, measured in this area

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env,
  so threaded `cargo test` cross-contaminates; a `gate_runner` test failed under
  `cargo test` and passed under nextest.
- Fixture git commands need `-c commit.gpgsign=false`; global signing is on and an
  unconfigured fixture hangs ~120s instead of failing.
- `Fixture::probe()` verifies the branch is merged into main. Committing on the
  feature branch inside a fixture makes it _unmerged_.
- Tests must not depend on a container runtime being installed **or** absent on
  the host — that is the very coupling this issue is about. Inject the PATH and
  the query result rather than probing the real machine.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — if it does, say
exactly that and stop. The primary runs all Rust validation, including clippy and
smoke, centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Both previous lanes in this chain shipped code that did not compile
— an ambiguous `Vec::new()`, a by-value row where a reference was wanted, a
`PathBuf` that was never imported — and the primary caught each. That is the
expected division of labour, so report honestly rather than optimistically.
