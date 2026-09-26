# Primary review + greenlight — THE-688

Reviewing row 606's investigation. **APPROVED. Implement the five-step plan as
written, with the two open decisions settled below.**

Good work on the part the brief asked for and could not answer itself: you
established that the predicate's **only** production use is the cleanup veto, and
that the other removal paths already do path-keyed layout deletion via
`workspace_remove::forget_worktree_path_in_db`
(`handlers/workspace_remove.rs:307-319`, `cmd/wt.rs:657-660`). That settles the
brief's "is it load-bearing elsewhere?" question with evidence.

You also found something the brief did not anticipate and were right to escalate
it rather than assume: `delete_tab_groups_for_worktree` is **session-scoped**
while `has_persisted_worktree_session` is **session-agnostic**.

## Decision 1 — remove the predicate outright

Take your primary recommendation, not the diagnostic-query alternative. Delete
the `WorkspaceStore` trait method (`store/workspace.rs:288-290`), the `Db`
implementation (`db_workspace.rs:37-46`), the cleanup use
(`merge_cleanup.rs:86-93`), and the now-obsolete core test
`persisted_worktree_session_guard_preserves_unknown_query_failure`.

Reason: a dead API still _shaped_ like an ownership check is a trap. The next
person to add a cleanup guard will find `has_persisted_worktree_session`, read
the name, and wire it straight back into a veto — which is exactly how this bug
got here. There is no caller to preserve it for.

## Decision 2 — all-session deletion, and do not assume the invariant

Add the new all-session operation, as you proposed. Do **not** reuse the
session-scoped one with a recorded `session_name`.

The primary checked the live database, and the shape is worth knowing:

```
SELECT worktree, count(DISTINCT session_name) n FROM tab_groups
GROUP BY worktree HAVING n>1;     -- one row, and it is the EMPTY worktree ('')
SELECT count(DISTINCT session_name) FROM tab_groups;   -- 24
```

So in practice a real worktree maps to exactly one `session_name` today — but:

- **`session_name` is the repo root path** (`/home/blake/code/thegn` for the held
  worktrees), _not_ a UI session id. Do not let the name mislead you into
  treating layout rows as per-UI-session and therefore ephemeral; they are not.
- The schema does not enforce one session per worktree — the primary key is
  `(session_name, name)` and `worktree` is a plain column, which is how a row
  with an **empty** worktree under three sessions already exists.

An all-session delete is never wrong and is sometimes more complete, so it is
the correct breadth — it matches the breadth of the predicate you are removing.
Keep the existing session-scoped operation untouched for `wt rm` / interactive
cleanup, as you planned. Delete dependent `group_tabs` rows too.

## Confirmed: the rest of the plan stands as written

Steps 1–5 are approved, including placing the layout deletion **inside the
existing post-removal transaction** next to `db.del_worktree(worktree)`
(`merge_lifecycle.rs:508-562`), and your explicit decision not to widen this into
rollback work if that transaction fails after physical deletion — surface a
bookkeeping error and keep the existing partial-bookkeeping behaviour. Do not
refactor that transaction.

Your safety list is exactly right and is the acceptance bar: after this change a
live latch or in-flight session-end (`automatic_cleanup_session_absent`), a
host-tenancy row, and a nonterminal dispatch must **all** still refuse in both
force modes, along with every existing identity/dirty/changed/resource/branch/
submodule/special-index/queue guard. `--force` bypasses the TTL clock and nothing
else.

**The live-session test is the one that makes this change safe.** You are removing
a check; the assertion that the remaining liveness guard still fires is what
proves the removal was redundant rather than load-bearing. Your plan registers a
process-local latch through `session_start_once` and releases it after — do that.
If it turns out not to be constructible, say so explicitly and do not quietly
ship without it.

## Scope

Out of scope, as you correctly identified: **THE-687**, running in parallel — do
not touch the missing-`result_oid` condition at `merge_sweep.rs:208-213`. Also
THE-685's filter-driver logic and THE-686's `StatusObservation` /
`Refusal::Changed` design; both landed and were reviewed. Preserve them.

Keep the report vocabulary as-is. `swept … (discarded build state)`,
`kept … — edited since landing`, `kept … — changed during cleanup` are the
contract THE-686 just established. A discarded layout does **not** need a new
report line — it is invisible bookkeeping, not something an operator loses.

## Validation

Your ordered plan is right, including running `just smoke` and grepping
`test/smoke.sh` for affected strings. That instinct is well earned: on THE-686 the
implementation was clippy-clean and passed 9106 unit tests, and smoke still caught
a case named `"sweep --force preserves ignored work"` that encoded the old
contract by name.

Attempt the cargo commands. **The pipeline sandbox mounts `/nix/store` read-only
and this usually fails outright** — if it does, say exactly that and stop. The
primary runs all Rust validation, including clippy and smoke, centrally. Never
report `implementation-ready` for code you could not compile; state what you could
not run. THE-686's implementation did not compile (an ambiguous `Vec::new()` in a
test) and the primary caught it — that is the expected division of labour, so
report honestly rather than optimistically.
