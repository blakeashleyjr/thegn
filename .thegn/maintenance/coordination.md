# Coordination brief — THE-692

Sixth and **largest** cause in the merged-worktree sweep chain. Five causes are
already fixed and landed — THE-685 (`0a3e3150`), THE-686 (`2acdf364`), THE-687 and
THE-688 (`e1a64f61`), THE-690 (`052a4bd9` + `6b7f7cc9`) — and `thegn merge sweep`
**now genuinely collects**: it swept 9 worktrees, the directories are gone, and a
second run reports "Nothing to sweep."

But the repo owner's sidebar is still full, because those 9 were the minority.

## What the primary already measured — build on it, do not redo it

```
worktrees: 47 ; with NO merge_queue row: 33
```

All 33 are merged into `main`. They can never be swept, because
`merge_sweep::landed_entries` selects candidates from exactly one place:

```rust
db.list_merge_queue()?.into_iter()
    .filter(|r| r.status == "landed" && r.target_branch == target)
```

and **`thegn land` never writes such a row.** `crates/thegn-host/src/cmd/land.rs`
contains no `record_merge_outcome`, no `persist_merge_outcome`, no
`update_merge_status`. Its only bookkeeping is
`merge_lifecycle::apply_landed_in_place_checked`, which (at
`merge_lifecycle.rs:162-180`) only files the worktree into a sidebar folder and
explicitly **bails** if the lifecycle ever asks for worktree removal.

So the landing is filed as _Merged_ in the sidebar and never enters the
grace-period lifecycle. `merged_ttl_secs` cannot apply to a row that does not
exist.

Re-verify these citations and report them confirmed (or moved); the primary does
not expect them to have changed.

## Why this one matters more than the other five

`thegn land` is the **canonical** landing path in this repo, not a side road.
CLAUDE.md is explicit: the canonical checkout's working tree is read-only, so
`git checkout main && git merge` fails, and `thegn land` is the documented
one-shot fold + gate + CAS ref advance. The fold-actor uses it for every landing.

So the recommended way to land produces worktrees the cleanup machinery
structurally cannot clean, while `merge add` + `merge drain` produces ones it can.
Identical end state in git; two different lifecycle outcomes. That asymmetry is
the bug.

## The identity is already in hand — no derivation needed

`AttemptOutcome::Landed { commit, resyncs }` (`integrate.rs:1443-1446`) carries
the fold tip now at the target ref, and `land.rs` already destructures it to print
`✓ landed {branch} → {target} @ {commit}`. So the non-empty `result_oid` that
THE-687 made mandatory for a landed row is available at exactly the point the row
should be written. **Do not** reach for THE-687's `derive_landed_commit` here;
that exists to repair rows written without an OID, and writing a fresh row that
needs repairing would be perverse.

## What to implement

Record a landed merge outcome (worktree, branch, target, `commit`) on
`AttemptOutcome::Landed`, through the **validated** writer, so a landed worktree
enters the same grace-period lifecycle a drained one does.

Hard requirements:

- **Validated writer only.** THE-687 added `landed ⇒ non-empty result_oid` guards
  to `replace_merge_status`/`update_merge_status` and the invariant in
  `db_aux.rs:119-124`. Go through them; do not hand-roll an INSERT.
- **No row for a land that did not land.** `Ready`, `Conflict`, gate-red and every
  refusal path must write nothing.
- **No row when the landed path is the repo root.** `apply_landed_in_place_checked`
  already guards that case; match it.
- **Keep `land`'s degraded-reporting discipline.** By this point git has already
  landed and the ref has already advanced. A DB failure must be surfaced as
  degraded post-land state — the file already does this for sidebar bookkeeping,
  with a comment explaining that a non-zero exit here invites a destructive retry.
  Follow that pattern exactly; do not turn a bookkeeping failure into a land
  failure.
- **The grace clock starts at the land.** `merge_sweep` reads `updated_at` as the
  clock (`landed_at: r.updated_at`), so the row's timestamp must be the land, not
  an earlier enqueue and not a later touch.

## A trap the primary hit today, in this exact area

THE-687's backfill wrote `SET result_oid=?, updated_at=?`. Because `updated_at`
**is** the grace clock, repairing a missing OID silently **reset the grace period**
on the four rows it fixed, pushing them out another seven days. The primary is
fixing that separately.

The lesson for you: any write to a landed row must be deliberate about
`updated_at`. Writing a _new_ row at land time should set it to now — that is the
clock starting. Touching an _existing_ landed row must not move it.

## Out of scope

- **THE-689** (accepted cleanup races) and **THE-691** (admission not uniform
  across the CLI). Both filed, both deferred. Do not absorb them.
- The five landed fixes in this chain. Preserve them; do not refactor.
- The `merge add` / `merge drain` queue path, TTL arithmetic, branch-retention
  holds (THE-596), and the OCI/tenancy/dispatch/layout guards.

## Acceptance criteria

- [ ] After `thegn land`, the worktree has a `merge_queue` row with
      `status='landed'`, the right `target_branch`, and the fold commit as
      `result_oid`.
- [ ] That worktree is swept once `merged_ttl_secs` has elapsed, every existing
      guard still applying.
- [ ] `Ready`, `Conflict` and gate-failure outcomes write no landed row.
- [ ] A land whose worktree is the repo root writes no row.
- [ ] A DB failure after the git land is reported as degraded and the land still
      exits zero.
- [ ] The grace clock starts at the land.
- [ ] Tests cover a land-then-sweep round trip and each non-landed outcome writing
      nothing.

## Testing traps, measured in this area today

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env, so
  threaded `cargo test` cross-contaminates.
- Fixture git commands need `-c commit.gpgsign=false`; global signing is on and an
  unconfigured fixture hangs ~120s instead of failing.
- **`merge_sweep::due` treats `merged_ttl_secs = 0` as "never sweep"** and returns
  no entries. A smoke helper set 0 while being named `sweep_fixture_due`, so every
  non-`--force` case it fed was vacuous and one was silently failing. For a
  clock-only assertion use `ttl = 1` and wait for real expiry.
- **`just smoke` is a real gate here.** It has caught two things in this chain that
  clippy and 9200+ unit tests did not.
- **A fixture written from the same mental model as the code proves nothing about
  the real interface.** THE-690's parser passed its unit tests and its smoke shim
  and still refused everything against the installed `docker`, because both
  fixtures modelled the parser's own assumption. If you touch anything that reads
  external output, capture the real thing.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host land`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — if it does, say exactly
that and stop. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in this chain shipped code that did not compile — an
ambiguous `Vec::new()`, a by-value row where a reference was wanted, a `PathBuf`
never imported, a `String` read of a nullable column — and the primary caught each
one. That is the expected division of labour, so report honestly.
