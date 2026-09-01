# Pipeline findings — the 2026-08-29/30 recovery and drain

Collected live while recovering a 121-row runaway and then draining 19 branches
to `main` through the agent pipeline. Every item below was **observed**, not
imagined: each names the row, branch, or command that produced it.

Status key: **[fixed]** landed in this change · **[filed]** has a Linear issue ·
**[open]** described, not yet built.

---

## A. What actually caused the runaway

### 1. The done-gate required a report nothing asked for **[fixed]**

`311d7e60` (THE-88, 2026-08-28 19:33) made a worker report mandatory for
`set-status done`. The deployed `[[pipeline.stages]]` prompts were never
updated: they never mentioned `dispatch report`, and never even referenced
`{row}`, so a worker could not have filed one. Every row dispatched after that
commit became unclosable without `--force`.

**Not one row in the entire 299-row roster had ever carried a report** — including
all 145 already closed `done`, which had been closed by a build predating the
gate. The config was individually valid at every other check.

Fixed by `config_pipeline::validate_stage_contracts`: a stage whose prompt omits
`{row}` or `dispatch report` is now a config error, surfaced at `config validate`
and at load.

### 2. `running` meant two opposite things **[fixed]**

A row's status could not distinguish a live worker from one that exited hours
ago into a row nobody closed. The supervisor filled slots by counting **live
daemon sessions**, so every exited-but-open row read as free capacity: 33 issues
ran against a configured budget of 9, and one worktree took eight successive
dispatches.

Fixed by schema v63's `exit_code`/`exited_at_ms` plus
`pipeline_run::row_liveness` (`Live` / `ExitedUnverified` / `Closed`), surfaced
as `running!exited` in `dispatch list`.

### 3. Slot accounting was not atomic **[fixed]**

`dispatch list` → judgment → `dispatch put` is a read-modify-write with no lock.
Fixed by `pipeline_claim` + `db_dispatch::claim_dispatch` inside
`BEGIN IMMEDIATE`, with `--allow-duplicate <reason>` as an auditable override.

### 4. An older runtime silently drove a newer database **[fixed]**

A v57 build operated a v62 database for hours, emitting **326,912** identical
mismatch warnings from one process, and `thegn --version` could not tell the two
builds apart. Fixed by `db::schema_refusal` (one actionable error) and a
`doctor` block printing the schema pair plus the CLI's and each daemon's actual
binary.

### 5. There were **zero** duplicate dispatches

Worth recording because the obvious fix is wrong. All 121 rows had distinct
artifacts; the "eight dispatches into one worktree" was a legitimate progression
(3 parallel chunks → 4 revision cycles → 1 security fix). Any dedupe keyed on
issue+stage+worktree alone would have destroyed real work — which is why
`pipeline_claim` keys on the **artifact** too.

---

## B. The dominant failure mode

### 6. Workers cannot commit **[filed: THE-91, Urgent]**

The sandbox grants the worktree `write` and its `.git` **`read`**. A git
worktree's `.git` is a pointer into the shared gitdir, so committing — the
pipeline's core contract — is forbidden. Row 392 said it outright:

> this environment cannot write the shared Git metadata, so the required merge
> commit cannot be created

It presents *dishonestly*, as a "silent worker death": rows 310, 320, 323, 324,
357, 375, 376, 377, 378, 379, 380, 381, 382, 383, 385, 386, 387, 389, 390, 391,
392, 393 all ended with the worker gone and no report. Roughly **half of all
rows**. In the later cases the artifact sat on disk `exists=yes tracked=no` with
**8–20 modified source files** uncommitted beside it — saved only because the
supervisor noticed and committed on the worker's behalf (~90 files across eight
branches).

### 7. `dispatch reap` — the join nobody should do by hand **[fixed]**

Detecting item 6 requires joining the roster, the daemon's live sessions, and
each row's git state. The supervisor did that ~15 times manually. Now one verb,
dry-run by default, which refuses to guess: an artifact committed with no report
is `NeedsDecision`, never auto-closed.

### 8. Losing uncommitted work is one cleanup away **[open]**

`dispatch report` should not be the first durable record. thegn should snapshot
or commit a worker's tree on exit, or the prompt must force the commit before
the long tail of the turn.

---

## C. Verification and gates

### 9. Reviewers passed on scoped tests; the fold gate then rejected them **[fixed]**

THE-11, THE-19 and THE-7 all reported "PASS, ready for the merge queue" and all
three were refused by `just test`. Scoped suites never run the workspace-level
coverage tests (`config_example`, `env_overlay_coverage`, `config_validate`) or
the architecture ratchets. THE-19's would have shipped an example config that
did not validate against its own new `[hooks]` schema.

Fixed in-prompt: a reviewer must run the full gate before reporting PASS, and a
red gate *is* the FAIL.

### 10. …which made review a heavy stage **[fixed, partially]**

With review concurrency 2 plus a land gate, the box ran 3–4 simultaneous
full-workspace runs; load hit 53, swap 9.5 GB, and reviews took 1.5–3.5 **hours**
instead of ~40 minutes. Review concurrency dropped to 1. Better: one shared,
serialized gate runner — the one `thegn land` already owns — instead of a gate
inside every reviewer.

### 11. Sandbox denials produce false FAILs **[filed: THE-90; mitigated in-prompt]**

`sccache: Operation not permitted` killed the gate before any test ran (row 320
reported FAIL for a branch with nothing wrong). Then `dns_filter` UDP tests,
bridge sockets and loopback listeners failed EPERM (rows 339, 349, 351, 360,
362) — and nextest's fail-fast cancelled **5,652** good tests behind two denied
ones, so the gate could never go green inside a sandbox.

Mitigated by giving reviewers a concrete exclusion command and a documented
BLOCKED-vs-FAIL distinction. The real fix is THE-90.

### 12. Load-sensitive tests are flaky tests **[fixed in THE-19]**

`background_descendant_cannot_hold_hook_completion_open` asserted wall-clock
timing and failed at 1.42 s against a 250 ms budget under load. Replaced with a
causality assertion, verified idle *and* under four-worker load.

---

## D. Merge-queue mechanics

### 13. A green gate goes stale the moment `main` moves **[open]**

THE-11 reported a green `just test` (7,126 passed), THE-27 landed, and THE-11's
very next land attempt was gate-red. THE-19 needed **six** rounds, three of them
purely base drift. Serial landing invalidates every queued branch that touches a
shared registry.

### 14. Land thrash is self-inflicted without a queue **[open]**

THE-20 merged `main` at `c28a507e`; while that merge ran, THE-17 landed, so
THE-20's next attempt conflicted again. Two failed lands for one branch purely
from ordering. `thegn integrate` folding the set in one gate run is the real fix.

### 15. Additive registries collide every single time **[open]**

`completion/catalog.rs`, `control.rs`, `cmd/mod.rs`, `platform/mod.rs`,
`lib.rs`, `store/mod.rs`, help pages, `control-v1.json`. Every branch appends;
every pair conflicts. Worth a merge driver.

### 16. Base drift breaks **compiles**, not just gates **[open]**

Row 352 hit four `E0063`s because landed branches added `ActionSpec` entries a
localization branch must supply keys for. THE-34 needed a hand-written error
taxonomy reconciliation; THE-22 needed a `NotificationKind::ALL` count bump.

### 17. Pinned counts trip branches that never touched the file **[open]**

`NotificationKind::ALL` (28→30), `marked_definition_count_is_pinned` (90→93),
`seen.len()`. Each is a correct ratchet and a guaranteed cross-branch conflict.

### 18. `thegn land` reports failure without the reason **[open]**

"breaks the build (gate red); not landed" — the operator must re-run the gate by
hand in the gate worktree to learn which test failed. Every time.

### 19. `thegn land` does not dequeue **[open]**

`tg/rad-parrot` landed and still showed `queued` in `thegn merge list` forever.

### 20. The Lead must perform merges itself **[open — worth productising]**

Because of THE-91. Doing it by hand took ~2 minutes per branch, and the check
that mattered was mechanical: extract each conflict hunk and assert
`theirs ⊆ ours` before taking `--ours`. That guard **correctly stopped** on
THE-34, where `main` genuinely had new content a blind `--ours` would have
deleted. It should be a verb (`thegn wt merge-main --verify-superset`), not a
script a supervisor rewrites each time.

---

## E. Schema allocation

### 21. `SCHEMA_VERSION` is a single racing integer **[open]**

THE-21 and THE-22 **both** claimed v64 while unlanded; this recovery's own
branch also claimed v63, and `main` used v63 for something else entirely.

### 22. An unlanded branch migrated the shared live database **[fixed]**

A worker's build — resolved from a worktree-local `target/debug` — migrated the
live DB to v64 while `main` was still v63, locking the supervisor's own CLI out
of the roster mid-recovery. Twice (64, then 65).

Fixed by `[database] migration_authority`, plus the bootstrap carve-out that
made it usable: at `user_version == 0` there is no controller to elect, so
authority governs **upgrades** only.

Still open: `dispatch report` *must* reach the live DB, so a worker cannot be
told to isolate `XDG_STATE_HOME` for it. That write should have a no-migrate
open mode.

---

## F. Worker behaviour and prompts

### 23. Workers echo inherited claims without re-verifying **[open]**

Row 308 repeated row 304's caveat verbatim. The supervisor reproduced it and
found it **real** — but only by running clippy directly. A worker repeating an
inherited claim should be required to re-verify it first.

### 24. Chunk scope blocks legitimate cross-cutting fixes **[open]**

THE-23 bounced twice on `repo_trust.rs` because chunk 3 forbade editing it. The
unblock was a hand-written Lead work order.

### 25. Lead work orders are the biggest manual cost **[open]**

Hand-written committed `.md` files for THE-23, THE-11, THE-19 (×3), THE-7,
THE-55, THE-20, THE-21, THE-22, THE-13, THE-17. A `thegn dispatch order` verb
should write, commit, and dispatch in one step.

### 26. The report is free text **[open]**

PASS / FAIL / BLOCKED / COMPLETE / "TARGETED PASS" / "PASS WITH TEST LIMITATION"
are only discoverable by reading prose. Rows 317 and 322 both reported BLOCKED
and both needed a hand-written authorization file. A structured verdict plus a
first-class "needs authorization" state would remove that.

### 27. The stage prompt silently outranks the work order **[open]**

Row 352's order explicitly asked for the full gate; the worker declined, citing
the `code` stage's "test minimally" default. Precedence needs to be stated, or
heavy-gate permission needs to be a per-dispatch flag.

### 28. Architects prescribe commands they never run **[open]**

Rows 333 and 335 were both told to run `cargo nextest run -p thegn-host --lib` —
`thegn-host` has no lib target. Rows 344 and 347 were given filters matching
**zero** tests, which pass vacuously: a worker can believe it verified something
when it ran nothing.

### 29. Every worker is the same expensive model **[open]**

All three roles share one `CODEX_HOME` (`gpt-5.6-luna`, reasoning `high`); no
`[[agents]].model` or per-stage `model` is set anywhere, though
`config.toml.example` documents the opposite intent. Evidence says tier it:
reviewers earned the high tier (credential leakage, OSC/DCS injection, a
source-fencing TOCTOU, symlink escape, terminal-control injection in audit
output). The merge-and-regate and lint-cleanup rounds did not.

---

## G. Smaller things

### 30. `.understand-anything/` made clean branches look dirty **[fixed]**

Untracked in every worktree, it made `thegn integrate` skip otherwise-clean
branches. Now gitignored. Separately, many reviewers reported the knowledge
graph absent though the tooling expects it.

### 31. `session open --stage` bypasses `claim_dispatch` **[open]**

The path the Lead actually uses writes its roster row directly, so the atomic
dedup and capacity enforcement are unused where they matter most.

### 32. Artifact naming defeats dedupe **[open]**

`session open --stage` names artifacts `<row>.md`, which is always unique, so
the claim's artifact key is inert unless `--chunk` is passed.

### 33. Launcher overrides leak into spawned tests **[fixed]**

`just live` exports `THEGN_DATABASE_MIGRATION_EXECUTABLE`; the
`automation_process` test inherited it and could not migrate its own isolated
database. A test that spawns thegn owns its whole environment.

---

## Recommended order

1. **THE-91** — workers cannot commit. Nothing else matters as much; it is half
   the rows.
2. **THE-90** — sandbox denials, so the queue-boundary gate is runnable.
3. **Batch folding** (13/14/15) — one gate run over the set instead of
   one-land-at-a-time restaling.
4. **Structured verdicts + `dispatch order`** (25/26) — the Lead's manual cost.
5. **Schema allocation** (21) and the no-migrate report write (22).
6. **Model tiering** (29) — cheap, and the evidence for it is already in.
