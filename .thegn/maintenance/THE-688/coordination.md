# Coordination brief — THE-688

Fourth cause in the merged-worktree sweep chain, and the one holding the most
worktrees (nine of thirteen). THE-685 (`0a3e3150`) and THE-686 (`2acdf364`) have
both **landed and are verified gone from the live sweep** — these nine were
unblocked by THE-686 and immediately hit this. The sweep still collects nothing,
which the repo owner hits every day.

## What the primary already verified — build on it, do not redo it

Measured today against the live database, for three of the held worktrees. The
refusal at `merge_cleanup.rs:86-93` tests three predicates, and it is the
**middle** one every time:

| predicate                        | backing table      | rows  |
| -------------------------------- | ------------------ | ----- |
| `has_cleanup_tenancy`            | `host_tenancy`     | **0** |
| `has_persisted_worktree_session` | `tab_groups`       | **1** |
| `has_cleanup_dispatch`           | `agent_dispatches` | **0** |

`has_persisted_worktree_session` (`thegn-core/src/db_workspace.rs:38-41`) is just
`SELECT EXISTS(SELECT 1 FROM tab_groups WHERE worktree=?1)`, and `tab_groups`
holds **persisted tab layout** — `session_name, name, kind, worktree, ordinal,
active_tab, instance_id, identity_state, quarantine_reason`. No pid, no lease, no
expiry. A row appears the first time a worktree is opened as a tab and never goes
away, so every worktree the operator has actually used is permanently
unsweepable. This database has 112 such rows.

**The real liveness guard already exists and already runs**, immediately after, at
`worktree_lifecycle.rs:1362-1371`: `automatic_cleanup_session_absent` consults the
in-process `session_runtime()` latches/`ending` map. That is actual current
ownership. The `tab_groups` check adds no safety on top of it — only a permanent
veto.

Re-verify these citations on your branch and report them as confirmed (or moved),
but the primary does not expect them to have changed.

## What to implement

1. **Stop treating a persisted layout as cleanup ownership.** Keep
   `has_cleanup_tenancy` (host placement) and `has_cleanup_dispatch` (an in-flight
   agent dispatch) — those are real claims — and keep
   `automatic_cleanup_session_absent` as the liveness guard, untouched.
2. **Tear the layout down with the worktree.** Delete that worktree's `tab_groups`
   rows as part of successful removal, alongside the cache/queue bookkeeping the
   sweep already does. A saved layout pointing at a path that no longer exists is
   an orphan, and leaving it behind is how this kind of row accumulates in the
   first place.
3. If you think the operator should still be told, make it a **report** line, never
   a refusal — and follow the shape THE-686 just established
   (`swept … (discarded build state)` / `kept … — edited since landing` /
   `kept … — changed during cleanup`). Do not invent a different vocabulary.

## Decide and justify: is `has_persisted_worktree_session` used anywhere else?

Before you change or delete it, find **every** caller. It may be load-bearing for
an interactive `wt rm` confirmation, where "this worktree has a saved layout" is
genuinely worth telling a human before they destroy it. If so, keep the function
and remove only its use as an _automatic-cleanup veto_ — do not delete a predicate
another surface depends on. Say in your report what the callers are.

## The hard safety line

A worktree with a **live** session must still never be swept, with or without
`--force`. You are removing a redundant check, not a real one: prove in a test
that the `worktree_lifecycle` guard still refuses. If you cannot construct that
test because the registry is in-process state, say so explicitly rather than
quietly shipping without it — that is the single assertion that makes this change
safe.

`--force` bypasses the TTL clock and nothing else.

## Out of scope

- **THE-687** — `landed commit identity is missing`, four rows with a NULL
  `result_oid`. Separate lane, running in parallel with yours; you will both touch
  the sweep area, so keep your diff narrow and do not touch `merge_sweep.rs:203`.
- THE-685's filter-driver logic and THE-686's status-observation /
  `Refusal::Changed` / `StatusObservation` design — both just landed, both
  reviewed. Preserve them; do not refactor them.
- TTL/grace arithmetic, branch-deletion holds, submodule and special-index guards,
  and `host_tenancy` / `agent_dispatches` semantics.

## Acceptance criteria (from the issue)

- [ ] A merged worktree previously open as a tab, with no live session, is swept
      once its TTL has elapsed.
- [ ] A worktree with a live session is still never swept, with or without `--force`.
- [ ] A worktree with a host-tenancy row or an in-flight dispatch is still never swept.
- [ ] Successful removal deletes that worktree's `tab_groups` rows; no orphan remains.
- [ ] Tests cover persisted-layout-only (swept), live session (kept), tenancy
      (kept), dispatch (kept) — **each with and without `--force`.**

That last one is a matrix, not four tests. Do not report the row finished with the
`--force` half missing.

## Testing traps, measured in this area today

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env, so
  threaded `cargo test` cross-contaminates — a `gate_runner` test failed under
  `cargo test` and passed under nextest.
- Fixture git commands need `-c commit.gpgsign=false`; global signing is on and an
  unconfigured fixture hangs ~120s instead of failing.
- `Fixture::probe()` verifies the branch is merged into main. Committing on the
  feature branch inside a fixture makes it _unmerged_.
- **`just smoke` is a real gate here.** THE-686 was clippy-clean and passed 9106
  unit tests, and `test/smoke.sh` still caught a case that encoded the old contract
  by name. If you change sweep behaviour or output, grep `test/smoke.sh` for the
  strings you touch.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only and this usually fails outright** — if it does, say
exactly that and stop. The primary runs all Rust validation, including clippy and
smoke, centrally. Never report `implementation-ready` for code you could not
compile; say what you could not run. Last lane's implementation did not compile
and the primary caught it — that is the expected division of labour, not a
failing, so report honestly.
