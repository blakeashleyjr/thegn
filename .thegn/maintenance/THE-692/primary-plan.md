# Primary review + greenlight — THE-692

Reviewing row 614's investigation. **APPROVED — implement the six-step plan, with
the escalated `UpToDate` boundary DECIDED BELOW, and it changes the shape of the
fix. Read that section first.**

Good work. The evidence is precise, the outcome/failure matrix is the right
artifact for this change, and you were right that observing before `land_branch`
and never deriving the OID from ancestry are the two things to get right. Steps
1–6 stand as written, subject to the decision below.

## DECISION — `UpToDate` must also record a row. This is the important part.

You asked the primary to preserve or change the boundary that only
`AttemptOutcome::Landed` writes a row. **Change it: `UpToDate` writes one too,
create-if-absent.**

Reasoning, from the live machine this issue was filed from:

- **`UpToDate` is precisely the state every already-accumulated worktree is in.**
  47 worktrees were registered here and 33 had no `merge_queue` row. They are all
  already contained in `main`, so `thegn land` on any of them returns `UpToDate`,
  not `Landed`. A fix scoped to `Landed` therefore repairs **nothing that already
  exists** — it only stops future lands adding to the pile.
- Re-running `thegn land` on a worktree you believe is finished is the natural
  operator gesture for "this is done, let the lifecycle have it". Today that
  gesture is silently inert.
- The safety proof is identical. `UpToDate` means the branch tip is contained in
  the target, which is the same ancestry fact the sweep independently re-proves
  before deleting anything. And THE-687 already made `UpToDate` carry a verified
  commit identity, so the non-empty `result_oid` the validated writer demands is
  in hand — no derivation, exactly as for `Landed`.

So the rule is:

> **Create a landed row when none exists. Never modify an existing landed row.**

The second half is not optional and is the subtlety that makes this safe:

- If a landed row already exists, leave it **entirely** alone — status, OID, and
  above all `updated_at`.
- `merge_sweep` reads `updated_at` as the grace-period clock
  (`landed_at: r.updated_at`). THE-687's backfill wrote a fresh `updated_at` and
  thereby **reset the grace period** on the four rows it repaired, pushing them out
  another seven days. That is THE-693, fixed separately today. Do not reintroduce
  the same defect from a different direction: an idempotent `thegn land` that
  re-stamps the clock would mean a worktree could never age out as long as anyone
  re-ran the command.
- For a genuinely new row, `updated_at` = now is correct. That is the clock
  starting, and for an `UpToDate` land it honestly records when the lifecycle first
  learned about it. Starting the clock late is a bounded cost; never starting it is
  the bug.

Report in your artifact what happens on a second consecutive `thegn land` of the
same worktree: it must be a no-op against the row.

## Confirmed as written

- Opening the DB read/write for every **non-root** manual land, because the row is
  required independently of sidebar `on_landed` policy. Good catch — the row is
  inert under `on_landed != expire` (the sweep returns early), but it must be
  correct rather than conditional.
- Root stays on the no-row path, matching `apply_landed_in_place_checked`'s
  existing guard.
- `persist_merge_outcome` only. Do not hand-roll SQL, and do not reach for
  `update_merge_status` for an absent row.
- Persist **before** sidebar lifecycle filing, and convert a post-CAS DB error to
  the existing warn-and-exit-zero discipline. `land.rs` already carries a comment
  explaining that a non-zero exit after git has landed invites a destructive
  retry — that reasoning is exactly right and now applies to your write too.
- `Ready`, `Conflict`, `GateFailed`, `GateError`, `Unreachable`, `RegistryChanged`,
  `QueueChanged` write no landed row. A land that did not land must leave no trace
  that could make the sweep delete something.

## Tests

Your step 5 and 6 matrix is the requirement. Three emphases:

1. **Add an `UpToDate` case** now that it is in scope: no row ⇒ a row is created
   with the verified OID; a row already present ⇒ untouched, and specifically its
   `updated_at` is unchanged. That second assertion is the THE-693 guard restated
   for this path — write it explicitly rather than assuming.
2. Your smoke round trip is the one that proves the issue. Use
   `merged_ttl_secs = 1` plus a real wait, **never 0**: `merge_sweep::due` treats
   `0` as _never sweep_ and returns no entries, which silently made an existing
   smoke case vacuous and in fact failing. Fixtures need
   `-c commit.gpgsign=false`.
3. Assert the exact fold OID, not merely that the field is non-empty. A wrong-but-
   present OID is the failure mode THE-687 spent a whole lane on.

## Out of scope

THE-689 and THE-691 (both filed, both deferred). The five landed fixes in this
chain — preserve them. The `merge add` / `merge drain` path, TTL arithmetic,
branch-retention holds, and the OCI/tenancy/dispatch/layout guards.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host land`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if so. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in this chain shipped something that did not compile —
an ambiguous `Vec::new()`, a by-value row where a reference was wanted, a
`PathBuf` never imported, a `String` read of a nullable column. The primary caught
each. That division of labour is expected, so report honestly rather than
optimistically.
